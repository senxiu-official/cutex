use super::*;
use crate::management::v2::notes::{handle, is_path, parse_body, PATH};
use serde_json::Value;

struct Fixture {
    root: PathBuf,
    store: NotesStore,
}

impl Fixture {
    fn new() -> Self {
        // Require the marked private home; never prepare or import real state.
        let home = std::env::var_os("CUTEX_TEST_PRIVATE_HOME")
            .map(PathBuf::from)
            .expect("notes tests require CUTEX_TEST_PRIVATE_HOME");
        assert!(home.join(".cutex-test-private-home").is_file());
        let root = home.join(format!("notes-test-{}", uuid::Uuid::new_v4()));
        let mut store = NotesStore::open(&root).unwrap();
        store.publish = Arc::new(|_| {});
        Self { root, store }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Only this fixture's explicitly created UUID directory is removed.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn session(id: &str) -> NoteScope {
    NoteScope::Session { id: id.to_string() }
}
fn project(id: &str) -> NoteScope {
    NoteScope::Project { id: id.to_string() }
}

fn create(nonce: &str, scope: NoteScope) -> CreateNote {
    CreateNote {
        client_nonce: nonce.to_string(),
        title: "Question summary".into(),
        body: "A note, never a native message.".into(),
        scope,
        pins: vec![],
        anchor: Some(NoteAnchor {
            session_id: "cutex.a".into(),
            thread_id: Some("native-thread-a".into()),
            turn_id: Some("turn-1".into()),
        }),
    }
}

fn query(path: &str) -> NotesQuery {
    NotesQuery::parse(path).unwrap()
}

fn ids(page: NotesPage) -> BTreeSet<String> {
    page.notes.into_iter().map(|note| note.id).collect()
}

#[test]
fn notes_store_persists_without_session_or_native_history_side_effects() {
    let fixture = Fixture::new();
    let first = fixture
        .store
        .create(create("nonce", session("cutex.a")))
        .unwrap();
    let reopened = NotesStore::open(&fixture.root).unwrap();
    let notes = reopened.list(&query(PATH), &[]).unwrap().notes;
    assert_eq!(notes, [first.clone()]);
    assert_eq!(first.revision, 1);
    assert_eq!(first.anchor_state, AnchorState::Ok);
    assert_eq!(first.anchor_state_at, None);
    assert_eq!(first.created_at, first.updated_at);
    assert_eq!(
        first.anchor.as_ref().unwrap().turn_id.as_deref(),
        Some("turn-1")
    );
    let names: Vec<_> = std::fs::read_dir(&fixture.root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(
        names.iter().all(|name| name.starts_with(DB_FILE)),
        "{names:?}"
    );
    let connection = fixture.store.connection().unwrap();
    let tables: Vec<String> = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(tables.len(), 3);
    assert!(tables.contains(&"notes".to_string()));
    assert!(tables.contains(&"note_pins".to_string()));
    assert!(tables.contains(&"note_nonces".to_string()));
}

#[test]
fn notes_pins_add_findability_without_replacing_natural_membership_or_source() {
    let fixture = Fixture::new();
    let mut input = create("nonce", session("cutex.a"));
    input.pins = vec![project("project-a"), NoteScope::Host {}, session("cutex.b")];
    let note = fixture.store.create(input).unwrap();
    let members_a = vec!["cutex.a".to_string()];
    for (path, members) in [
        ("/v2/notes?scope=session&id=cutex.a", vec![]),
        ("/v2/notes?scope=session&id=cutex.b", vec![]),
        ("/v2/notes?scope=project&id=project-a", vec![]),
        ("/v2/notes?scope=project&id=project-b", members_a.clone()),
        ("/v2/notes", vec![]),
    ] {
        assert_eq!(
            ids(fixture.store.list(&query(path), &members).unwrap()),
            BTreeSet::from([note.id.clone()])
        );
    }
    assert!(fixture
        .store
        .list(
            &query("/v2/notes?scope=session&id=cutex.a&pinnedOnly=true"),
            &[]
        )
        .unwrap()
        .notes
        .is_empty());
    assert_eq!(
        fixture
            .store
            .list(
                &query("/v2/notes?scope=project&id=project-a&pinnedOnly=true"),
                &[]
            )
            .unwrap()
            .notes
            .len(),
        1
    );
    assert!(fixture
        .store
        .list(
            &query("/v2/notes?scope=project&id=project-b&pinnedOnly=true"),
            &members_a
        )
        .unwrap()
        .notes
        .is_empty());
    let unpinned = fixture
        .store
        .patch(
            &note.id,
            PatchNote {
                expected_revision: 1,
                title: None,
                body: None,
                pins: Some(vec![]),
            },
        )
        .unwrap();
    assert_eq!(unpinned.scope, note.scope);
    assert_eq!(unpinned.anchor, note.anchor);
    assert_eq!(unpinned.created_at, note.created_at);
    assert!(fixture
        .store
        .list(&query("/v2/notes?scope=project&id=project-a"), &[])
        .unwrap()
        .notes
        .is_empty());
    assert_eq!(
        fixture
            .store
            .list(&query("/v2/notes?scope=project&id=project-a"), &members_a)
            .unwrap()
            .notes
            .len(),
        1
    );
    assert_eq!(
        fixture.store.list(&query(PATH), &[]).unwrap().notes.len(),
        1
    );
    assert!(fixture
        .store
        .list(&query("/v2/notes?pinnedOnly=true"), &[])
        .unwrap()
        .notes
        .is_empty());
}

#[test]
fn notes_retire_restore_and_fork_do_not_reanchor_copy_or_delete() {
    let fixture = Fixture::new();
    let note = fixture
        .store
        .create(create("original", session("cutex.a")))
        .unwrap();
    // No roster row or native turn needs to exist to retrieve an older note.
    assert_eq!(
        fixture
            .store
            .list(&query("/v2/notes?scope=session&id=cutex.a"), &[])
            .unwrap()
            .notes,
        [note.clone()]
    );
    assert!(fixture
        .store
        .list(&query("/v2/notes?scope=session&id=cutex.fork"), &[])
        .unwrap()
        .notes
        .is_empty());
    assert_eq!(fixture.store.list(&query(PATH), &[]).unwrap().notes, [note]);
}

#[test]
fn notes_nonce_replay_is_canonical_and_cannot_resurrect_a_deleted_note() {
    let fixture = Fixture::new();
    let mut input = create("nonce", session("cutex.a"));
    input.pins = vec![project("project-a"), NoteScope::Host {}];
    let note = fixture.store.create(input.clone()).unwrap();
    input.pins.reverse();
    assert_eq!(fixture.store.create(input.clone()).unwrap(), note);
    let changed = fixture
        .store
        .patch(
            &note.id,
            PatchNote {
                expected_revision: 1,
                title: Some("Edited".into()),
                body: None,
                pins: None,
            },
        )
        .unwrap();
    assert_eq!(fixture.store.create(input.clone()).unwrap(), changed);
    let mut conflict = input.clone();
    conflict.body = "different input".into();
    assert!(matches!(
        fixture.store.create(conflict),
        Err(NotesError::NonceConflict)
    ));
    fixture.store.delete(&note.id, 2).unwrap();
    assert!(matches!(
        fixture.store.create(input),
        Err(NotesError::Deleted)
    ));
    assert!(fixture
        .store
        .list(&query(PATH), &[])
        .unwrap()
        .notes
        .is_empty());
    let connection = fixture.store.connection().unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM note_pins", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM note_nonces", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn notes_revision_conflict_preserves_edit_and_blocks_stale_delete() {
    let fixture = Fixture::new();
    let note = fixture
        .store
        .create(create("nonce", NoteScope::Host {}))
        .unwrap();
    let changed = fixture
        .store
        .patch(
            &note.id,
            PatchNote {
                expected_revision: 1,
                title: None,
                body: Some("owner edit".into()),
                pins: None,
            },
        )
        .unwrap();
    assert_eq!(changed.revision, 2);
    assert!(matches!(
        fixture.store.patch(
            &note.id,
            PatchNote {
                expected_revision: 1,
                title: Some("stale".into()),
                body: None,
                pins: None
            }
        ),
        Err(NotesError::RevisionConflict {
            current_revision: 2
        })
    ));
    assert!(matches!(
        fixture.store.delete(&note.id, 1),
        Err(NotesError::RevisionConflict {
            current_revision: 2
        })
    ));
    assert_eq!(
        fixture.store.list(&query(PATH), &[]).unwrap().notes,
        [changed]
    );
    fixture.store.delete(&note.id, 2).unwrap();
    assert!(matches!(
        fixture.store.delete(&note.id, 2),
        Err(NotesError::NotFound)
    ));
}

#[test]
fn notes_concurrent_creates_and_compare_and_swap_are_atomic() {
    use std::sync::{Arc, Barrier};
    let fixture = Fixture::new();
    let barrier = Arc::new(Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let store = fixture.store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .create(create("same-nonce", NoteScope::Host {}))
                    .unwrap()
            })
        })
        .collect();
    let notes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert!(notes.iter().all(|note| note.id == notes[0].id));
    let barrier = Arc::new(Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let store = fixture.store.clone();
            let id = notes[0].id.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.patch(
                    &id,
                    PatchNote {
                        expected_revision: 1,
                        title: Some(format!("edit {i}")),
                        body: None,
                        pins: None,
                    },
                )
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(NotesError::RevisionConflict {
                    current_revision: 2
                })
            ))
            .count(),
        3
    );
}

#[test]
fn notes_search_pagination_is_literal_bounded_and_filter_bound() {
    let fixture = Fixture::new();
    for i in 0..5 {
        let mut input = create(&format!("nonce-{i}"), NoteScope::Host {});
        input.title = format!("共享 LITERAL %_ {i}");
        fixture.store.create(input).unwrap();
    }
    fixture
        .store
        .create(create("unrelated", NoteScope::Host {}))
        .unwrap();
    let path = "/v2/notes?query=literal&limit=2";
    let mut page = fixture.store.list(&query(path), &[]).unwrap();
    assert_eq!(page.notes.len(), 2);
    let mut found = BTreeSet::new();
    loop {
        for note in page.notes {
            assert!(found.insert(note.id));
        }
        let Some(cursor) = page.next_cursor else {
            break;
        };
        assert!(NotesQuery::parse(&format!(
            "/v2/notes?scope=project&id=project-a&cursor={cursor}"
        ))
        .is_err());
        page = fixture
            .store
            .list(&query(&format!("{path}&cursor={cursor}")), &[])
            .unwrap();
    }
    assert_eq!(found.len(), 5);
    assert_eq!(
        fixture
            .store
            .list(&query("/v2/notes?query=%25_"), &[])
            .unwrap()
            .notes
            .len(),
        5
    );
    assert_eq!(
        fixture
            .store
            .list(&query("/v2/notes?query=共享"), &[])
            .unwrap()
            .notes
            .len(),
        5
    );
}

#[test]
fn notes_requests_reject_unknown_fields_duplicates_nulls_and_oversize_values() {
    for path in [
        "/v2/notes?scope=bad",
        "/v2/notes?scope=session",
        "/v2/notes?id=x",
        "/v2/notes?limit=0",
        "/v2/notes?limit=201",
        "/v2/notes?limit=2&limit=3",
        "/v2/notes?pinnedOnly=1",
        "/v2/notes?wat=1",
        "/v2/notes?cursor=bad",
    ] {
        assert!(NotesQuery::parse(path).is_err(), "{path}");
    }
    assert!(NotesQuery::parse(&format!("/v2/notes?query={}", "字".repeat(501))).is_err());
    for body in [
        r#"{"expectedRevision":1,"body":null}"#,
        r#"{"expectedRevision":1,"scope":{"kind":"host"}}"#,
        r#"{"expectedRevision":1,"anchor":null}"#,
        r#"{"expectedRevision":1,"pins":[{"kind":"host","id":"x"}]}"#,
        r#"{"expectedRevision":1,"body":"first","body":"second"}"#,
    ] {
        assert!(parse_body::<PatchNote>(body.as_bytes()).is_err(), "{body}");
    }
    assert!(parse_body::<PatchNote>(br#"{"expectedRevision":1}"#)
        .unwrap()
        .validated()
        .is_err());
    assert!(
        parse_body::<PatchNote>(br#"{"expectedRevision":0,"body":"x"}"#)
            .unwrap()
            .validated()
            .is_err()
    );
    let input = create("nonce", NoteScope::Host {});
    let mut invalid = input.clone();
    invalid.body = "字".repeat(64_001);
    assert!(invalid.validated().is_err());
    let mut invalid = input.clone();
    invalid.title = "x".repeat(201);
    assert!(invalid.validated().is_err());
    let mut invalid = input.clone();
    invalid.client_nonce = "x".repeat(129);
    assert!(invalid.validated().is_err());
    let mut invalid = input.clone();
    invalid.pins = vec![NoteScope::Host {}; 2];
    assert!(invalid.validated().is_err());
    let mut invalid = input.clone();
    invalid.body = "\0".into();
    assert!(invalid.validated().is_err());
    let mut accepted = input;
    accepted.title.clear();
    accepted.body = "字".repeat(64_000);
    assert!(accepted.validated().is_ok());
    assert!(
        is_path(PATH)
            && is_path("/v2/notes/note-x")
            && !is_path("/v2/notes/x/more")
            && !is_path("/v2/notes/")
    );
}

#[test]
fn notes_unknown_database_version_is_not_overwritten() {
    let fixture = Fixture::new();
    let connection = fixture.store.connection().unwrap();
    connection.pragma_update(None, "user_version", 99).unwrap();
    assert!(NotesStore::open(&fixture.root).is_err());
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        99
    );
}

#[cfg(unix)]
#[test]
fn notes_database_and_sidecars_are_owner_private_and_symlinks_refused() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let fixture = Fixture::new();
    let connection = fixture.store.connection().unwrap();
    fixture
        .store
        .create(create("nonce", NoteScope::Host {}))
        .unwrap();
    for entry in std::fs::read_dir(&fixture.root).unwrap() {
        let entry = entry.unwrap();
        assert_eq!(entry.metadata().unwrap().permissions().mode() & 0o077, 0);
    }
    assert_eq!(
        std::fs::metadata(&fixture.root)
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
    drop(connection);
    let linked = fixture.root.join("linked");
    std::fs::create_dir(&linked).unwrap();
    symlink(fixture.root.join(DB_FILE), linked.join(DB_FILE)).unwrap();
    assert!(NotesStore::open(&linked).is_err());
}

fn http(method: &str, path: &str, body: &str) -> (u16, Value) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let raw = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let client = std::thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    });
    let (mut stream, _) = listener.accept().unwrap();
    let request = crate::http::server::read_simple_http_request(&mut stream).unwrap();
    handle(&mut stream, &request).unwrap();
    drop(stream);
    let response = client.join().unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    (
        head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(body).unwrap(),
    )
}

#[test]
fn notes_http_crud_errors_and_revision_contract() {
    let home = std::env::var_os("CUTEX_TEST_PRIVATE_HOME")
        .map(PathBuf::from)
        .unwrap();
    assert!(home.join(".cutex-test-private-home").is_file());
    let scope_id = format!("notes-http-{}", uuid::Uuid::new_v4());
    let input = create(&uuid::Uuid::new_v4().to_string(), session(&scope_id));
    let body = serde_json::to_string(&input).unwrap();
    let (status, note) = http("POST", PATH, &body);
    assert_eq!(status, 200, "{note}");
    let id = note["id"].as_str().unwrap();
    let path = format!("{PATH}/{id}");
    assert_eq!(http("GET", &path, ""), (200, note.clone()));
    assert_eq!(note["anchorState"], "ok");
    assert_eq!(note["anchorStateAt"], Value::Null);
    assert_eq!(http("POST", PATH, &body), (200, note.clone()));
    let (status, page) = http("GET", &format!("{PATH}?scope=session&id={scope_id}"), "");
    assert_eq!(status, 200);
    assert_eq!(page["notes"], json!([note.clone()]));
    assert_eq!(page["nextCursor"], Value::Null);
    let (status, edited) = http(
        "PATCH",
        &path,
        r#"{"expectedRevision":1,"body":"owner edit"}"#,
    );
    assert_eq!(status, 200);
    assert_eq!(edited["revision"], 2);
    let (status, conflict) = http("DELETE", &path, r#"{"expectedRevision":1}"#);
    assert_eq!(status, 409);
    assert!(conflict.to_string().contains("note_revision_conflict"));
    assert_eq!(
        http("PATCH", &path, r#"{"expectedRevision":2,"body":null}"#).0,
        400
    );
    assert_eq!(
        http("DELETE", &path, r#"{"expectedRevision":2}"#),
        (200, json!({"deleted":true}))
    );
    assert_eq!(http("POST", PATH, &body).0, 409);
    assert_eq!(http("GET", &path, "").0, 404);
    assert_eq!(http("GET", &format!("{path}?scope=host"), "").0, 400);
    assert_eq!(http("GET", "/v2/notes/invalid-id", "").0, 400);
    assert_eq!(http("DELETE", &path, r#"{"expectedRevision":2}"#).0, 404);
    assert_eq!(http("PUT", &path, "{}").0, 405);
    assert_eq!(http("POST", PATH, r#"{"unknown":true}"#).0, 400);
    assert_eq!(http("GET", "/v2/notes?limit=999", "").0, 400);
}

#[test]
fn notes_get_is_a_snapshot_and_missing_ids_are_explicit() {
    let fixture = Fixture::new();
    let mut input = create("get", session("cutex.a"));
    input.pins = vec![project("pinned-project"), NoteScope::Host {}];
    let note = fixture.store.create(input).unwrap();
    assert_eq!(fixture.store.get(&note.id).unwrap(), note);
    assert!(matches!(
        fixture.store.get("bad"),
        Err(NotesError::Invalid(_))
    ));
    assert!(matches!(
        fixture.store.get(&format!("note-{}", uuid::Uuid::new_v4())),
        Err(NotesError::NotFound)
    ));
}

#[test]
fn notes_changes_publish_once_after_commit_without_private_body() {
    use std::sync::Mutex;
    let mut fixture = Fixture::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let root = fixture.root.clone();
    fixture.store.publish = Arc::new(move |change| {
        // Another connection must see the committed value at notification time.
        let current = NotesStore::open(&root).unwrap().get(&change.note_id);
        if change.change == NoteChangeKind::Deleted {
            assert!(matches!(current, Err(NotesError::NotFound)));
        } else {
            assert_eq!(current.unwrap().revision, change.revision);
        }
        captured.lock().unwrap().push(change);
    });
    let input = create("events", session("cutex.a"));
    let created = fixture.store.create(input.clone()).unwrap();
    assert_eq!(fixture.store.create(input).unwrap(), created);
    let updated = fixture
        .store
        .patch(
            &created.id,
            PatchNote {
                expected_revision: 1,
                title: Some("Edited".into()),
                body: None,
                pins: None,
            },
        )
        .unwrap();
    assert!(fixture.store.delete(&created.id, 1).is_err());
    fixture.store.delete(&created.id, updated.revision).unwrap();
    let events = events.lock().unwrap();
    assert_eq!(
        events.iter().map(|e| e.change).collect::<Vec<_>>(),
        [
            NoteChangeKind::Created,
            NoteChangeKind::Updated,
            NoteChangeKind::Deleted
        ]
    );
    assert_eq!(
        events.iter().map(|e| e.revision).collect::<Vec<_>>(),
        [1, 2, 2]
    );
    for event in events.iter() {
        let params = serde_json::to_value(event).unwrap();
        assert_eq!(params.as_object().unwrap().len(), 5);
        assert!(params.get("body").is_none() && params.get("title").is_none());
        crate::management::v2::contract_validation::validate_cutex_event_message(
            &json!({"method": CHANGED_EVENT,"params":params}),
        )
        .unwrap();
        let mut leaked = params;
        leaked["body"] = json!("must not publish");
        assert!(
            crate::management::v2::contract_validation::validate_cutex_event_message(
                &json!({"method": CHANGED_EVENT,"params":leaked})
            )
            .is_err()
        );
    }
}

#[test]
fn notes_removed_turn_ids_mark_only_matching_anchors_and_notify_once_each() {
    use std::sync::Mutex;
    let mut fixture = Fixture::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    fixture.store.publish = Arc::new(move |event| captured.lock().unwrap().push(event));
    let mut notes = Vec::new();
    for (nonce, source, turn) in [
        ("old", "cutex.a", "turn-1"),
        ("selected", "cutex.a", "turn-2"),
        ("newer", "cutex.a", "turn-3"),
        ("other", "cutex.b", "turn-2"),
    ] {
        let mut input = create(nonce, session(source));
        input.anchor.as_mut().unwrap().session_id = source.into();
        input.anchor.as_mut().unwrap().turn_id = Some(turn.into());
        notes.push(fixture.store.create(input).unwrap());
    }
    events.lock().unwrap().clear();
    let removed = vec!["turn-2".into(), "turn-3".into()];
    assert_eq!(fixture.store.mark_reverted("cutex.a", &removed).unwrap(), 2);
    assert_eq!(fixture.store.mark_reverted("cutex.a", &removed).unwrap(), 0);
    assert_eq!(fixture.store.mark_reverted("cutex.a", &[]).unwrap(), 0);
    for (i, original) in notes.iter().enumerate() {
        let note = fixture.store.get(&original.id).unwrap();
        assert_eq!(note.anchor, original.anchor);
        assert_eq!(note.scope, original.scope);
        assert_eq!(note.body, original.body);
        if [1, 2].contains(&i) {
            assert_eq!(note.anchor_state, AnchorState::Reverted);
            assert_eq!(note.revision, 2);
            assert!(
                chrono::DateTime::parse_from_rfc3339(note.anchor_state_at.as_ref().unwrap())
                    .is_ok()
            );
            assert_eq!(note.anchor_state_at.as_ref(), Some(&note.updated_at));
            assert!(matches!(
                fixture.store.delete(&note.id, 1),
                Err(NotesError::RevisionConflict {
                    current_revision: 2
                })
            ));
        } else {
            assert_eq!(note, *original);
        }
    }
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| e.change == NoteChangeKind::Anchor));
}

#[test]
fn notes_session_delete_retains_content_and_overrides_reverted_state() {
    use std::sync::Mutex;
    let mut fixture = Fixture::new();
    let anchored = fixture
        .store
        .create(create("anchor", project("project-a")))
        .unwrap();
    let mut unanchored = create("scope-only", session("cutex.a"));
    unanchored.anchor = None;
    let unanchored = fixture.store.create(unanchored).unwrap();
    let mut elsewhere = create("elsewhere", session("cutex.a"));
    elsewhere.anchor.as_mut().unwrap().session_id = "cutex.b".into();
    let elsewhere = fixture.store.create(elsewhere).unwrap();
    fixture
        .store
        .mark_reverted("cutex.a", &["turn-1".into()])
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    fixture.store.publish = Arc::new(move |event| captured.lock().unwrap().push(event));
    assert_eq!(fixture.store.mark_session_deleted("cutex.a").unwrap(), 2);
    assert_eq!(fixture.store.mark_session_deleted("cutex.a").unwrap(), 0);
    assert_eq!(
        fixture
            .store
            .mark_reverted("cutex.a", &["turn-1".into()])
            .unwrap(),
        0
    );
    for original in [&anchored, &unanchored] {
        let note = fixture.store.get(&original.id).unwrap();
        assert_eq!(note.anchor_state, AnchorState::SessionDeleted);
        assert!(note.anchor_state_at.is_some());
        assert_eq!(note.anchor, original.anchor);
        assert_eq!(note.scope, original.scope);
        assert_eq!(note.body, original.body);
    }
    assert_eq!(fixture.store.get(&anchored.id).unwrap().revision, 3);
    assert_eq!(fixture.store.get(&elsewhere.id).unwrap(), elsewhere);
    assert_eq!(
        fixture.store.list(&query(PATH), &[]).unwrap().notes.len(),
        3
    );
    assert_eq!(events.lock().unwrap().len(), 2);
}

#[test]
fn notes_v1_migration_preserves_records_pins_nonce_and_revision() {
    let fixture = Fixture::new();
    let root = fixture.root.join("legacy");
    crate::management::client_tokens::ensure_private_dir(&root).unwrap();
    secure_file(&root.join(DB_FILE), true).unwrap();
    let connection = Connection::open(root.join(DB_FILE)).unwrap();
    connection.execute_batch("CREATE TABLE notes (id TEXT PRIMARY KEY,title TEXT NOT NULL,body TEXT NOT NULL,scope_kind TEXT NOT NULL,scope_id TEXT NOT NULL,anchor TEXT,revision INTEGER NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);
      CREATE TABLE note_pins (note_id TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,kind TEXT NOT NULL,scope_id TEXT NOT NULL,PRIMARY KEY(note_id,kind,scope_id));
      CREATE TABLE note_nonces (nonce TEXT PRIMARY KEY,request_hash TEXT NOT NULL,note_id TEXT NOT NULL); PRAGMA user_version=1;").unwrap();
    let mut input = create("v1", session("cutex.a"));
    input.pins = vec![project("project-a")];
    let input = input.validated().unwrap();
    let id = format!("note-{}", uuid::Uuid::new_v4());
    let timestamp = now();
    connection
        .execute(
            "INSERT INTO notes VALUES(?1,?2,?3,'session','cutex.a',?4,7,?5,?5)",
            params![
                id,
                input.title,
                input.body,
                serde_json::to_string(&input.anchor).unwrap(),
                timestamp
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO note_pins VALUES(?1,'project','project-a')",
            [&id],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO note_nonces VALUES(?1,?2,?3)",
            params![
                input.client_nonce,
                digest(&serde_json::to_vec(&input).unwrap()),
                id
            ],
        )
        .unwrap();
    drop(connection);
    let store = NotesStore::open(root).unwrap();
    let note = store.get(&id).unwrap();
    assert_eq!(note.anchor_state, AnchorState::Ok);
    assert_eq!(note.anchor_state_at, None);
    assert_eq!(note.revision, 7);
    assert_eq!(note.pins, input.pins);
    assert_eq!(note.created_at, timestamp);
    assert_eq!(store.create(input).unwrap(), note);
    assert_eq!(
        store
            .connection()
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn notes_concurrent_first_open_migrates_once() {
    use std::sync::Barrier;
    let fixture = Fixture::new();
    let root = fixture.root.join("concurrent");
    let barrier = Arc::new(Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                NotesStore::open(root).unwrap();
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    assert!(NotesStore::open(root)
        .unwrap()
        .list(&query(PATH), &[])
        .unwrap()
        .notes
        .is_empty());
}

#[test]
fn notes_default_lifecycle_hooks_publish_metadata_to_host_channel_only() {
    use crate::management::v2::repository::{management_v2_repository, ReplayQuery};
    use crate::management::v2::services::HOST_EVENT_SESSION_ID;
    // The default event repository is process-global. Isolate this integration
    // check from unrelated tests that temporarily change HOME or initialize it.
    if std::env::var_os("CUTEX_NOTES_HOOK_TEST_CHILD").is_none() {
        let fixture = Fixture::new();
        let home = fixture.root.join("hook-home");
        crate::management::client_tokens::ensure_private_dir(&home).unwrap();
        std::fs::write(home.join(".cutex-test-private-home"), "notes hook test").unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "notes::store::tests::notes_default_lifecycle_hooks_publish_metadata_to_host_channel_only", "--nocapture", "--test-threads=1"])
            .env("CUTEX_NOTES_HOOK_TEST_CHILD", "1")
            .env("CUTEX_TEST_PRIVATE_HOME", &home)
            .env("HOME", &home)
            .output().unwrap();
        assert!(
            output.status.success(),
            "isolated hook test failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // Before the first note, the hooks must not create the notes store.
    let notes_dir = crate::config::paths::config_dir().unwrap().join("notes");
    assert!(!NotesStore::default_exists());
    mark_reverted_default("never-noted", &["turn-1".into()]);
    mark_session_deleted_default("never-noted");
    assert!(!notes_dir.exists(), "hooks created {}", notes_dir.display());
    let repository = management_v2_repository().unwrap();
    let metadata = repository.stream_metadata().unwrap();
    let after = metadata.latest.map(|boundary| boundary.cursor);
    let store = NotesStore::open_default().unwrap();
    assert!(NotesStore::default_exists());
    let session_id = format!("notes-hooks-{}", uuid::Uuid::new_v4());
    let mut input = create(&uuid::Uuid::new_v4().to_string(), session(&session_id));
    input.anchor.as_mut().unwrap().session_id = session_id.clone();
    let created = store.create(input.clone()).unwrap();
    store.create(input).unwrap();
    mark_reverted_default(&session_id, &["turn-1".into()]);
    mark_reverted_default(&session_id, &["turn-1".into()]);
    assert_eq!(
        store.get(&created.id).unwrap().anchor_state,
        AnchorState::Reverted
    );
    mark_session_deleted_default(&session_id);
    mark_session_deleted_default(&session_id);
    assert_eq!(
        store.get(&created.id).unwrap().anchor_state,
        AnchorState::SessionDeleted
    );
    let edited = store
        .patch(
            &created.id,
            PatchNote {
                expected_revision: 3,
                title: Some("Saved after source deletion".into()),
                body: None,
                pins: None,
            },
        )
        .unwrap();
    assert_eq!(edited.anchor_state, AnchorState::SessionDeleted);
    store.delete(&created.id, 4).unwrap();
    let page = repository
        .page(ReplayQuery {
            after,
            stream_id: Some(metadata.stream_id),
            limit: 100,
            cutex_session_id: Some(HOST_EVENT_SESSION_ID.into()),
            ..Default::default()
        })
        .unwrap();
    let events: Vec<_> = page
        .events
        .iter()
        .filter(|event| {
            event.cutex.as_ref().is_some_and(|message| {
                message.method == CHANGED_EVENT && message.params["noteId"] == created.id
            })
        })
        .collect();
    let changes: Vec<_> = events
        .iter()
        .map(|event| {
            event.cutex.as_ref().unwrap().params["change"]
                .as_str()
                .unwrap()
        })
        .collect();
    assert_eq!(
        changes,
        ["created", "anchor", "anchor", "updated", "deleted"]
    );
    for event in events {
        assert_eq!(event.cutex_session_id, HOST_EVENT_SESSION_ID);
        assert!(event.native.is_none());
        assert!(event.correlation.thread_id.is_none());
        assert!(!serde_json::to_string(event)
            .unwrap()
            .contains(&created.body));
    }
    assert!(repository
        .page(ReplayQuery {
            cutex_session_id: Some(session_id),
            ..Default::default()
        })
        .unwrap()
        .events
        .is_empty());
}
