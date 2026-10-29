// Private fixtures for the exact no-settlement r265 predecessor contracts.
use serde_json::json;

const AUTHORITY: &str = "runtime/agent-management/v1/agent-management-v1.json";
fn marker_fixture() -> Value {
    json!({"minimum_reader_release":"r264","binary_only_rollback_blocked_to":["r263"],
        "reason":"archive_receipt_fields_and_permanent_enum_variants"})
}
fn archive_fixture() -> Value {
    json!({"request":{"action_id":"fixture","review":{"cutex_session_id":"cutex.fixture",
        "formal_name":"Fixture","operation":"permanent_retire","revision":1,"runtime_generation":7}},
        "request_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "stage":"permanent_retirement_committed","result":null,"error":null,
        "committed_at":"2026-10-26T00:00:00Z","archiveKind":"permanent","restorable":false})
}
fn authority_fixture() -> Value {
    json!({"schema":"cutex/agent-management-store/v1","store_revision":0,"projects":{},
        "agents":{},"actions":{},"authority_receipts":{},"failure_events":{},
        "archive_storage_contract_v1":marker_fixture(),
        "agent_archive_actions":{"fixture":archive_fixture()},
        "agent_archive_audit":{"fixture":archive_fixture()}})
}

#[test]
fn r264_closed_archive_contract_accepts_baseline_variants() {
    for operation in ["archive", "restore", "permanent_retire"] {
        for stage in [
            "prepared",
            "stopped",
            "committed",
            "stopped_not_archived",
            "uncertain",
            "permanent_retirement_committed",
        ] {
            let mut receipt = archive_fixture();
            receipt["request"]["review"]["operation"] = json!(operation);
            receipt["stage"] = json!(stage);
            archive_receipt(&receipt).unwrap();
        }
    }
    for kind in [
        json!(null),
        json!("active"),
        json!("soft"),
        json!("permanent"),
    ] {
        for restorable in [json!(null), json!(false), json!(true)] {
            let mut receipt = archive_fixture();
            receipt["archiveKind"] = kind.clone();
            receipt["restorable"] = restorable;
            archive_receipt(&receipt).unwrap();
        }
    }
    let mut old = archive_fixture();
    old.as_object_mut().unwrap().remove("archiveKind");
    old.as_object_mut().unwrap().remove("restorable");
    archive_receipt(&old).unwrap();
}

#[test]
fn r264_closed_archive_contract_rejects_future_and_corrupt_values() {
    for (pointer, value) in [
        ("/stage", json!("future")),
        ("/archiveKind", json!("future")),
        ("/request/review/operation", json!("future")),
        ("/restorable", json!(1)),
        ("/request_sha256", json!("bad")),
        ("/committed_at", json!("bad")),
        ("/request/review/revision", json!(-1)),
        ("/result", json!(false)),
    ] {
        let mut receipt = archive_fixture();
        *receipt.pointer_mut(pointer).unwrap() = value;
        assert!(archive_receipt(&receipt).is_err(), "{pointer}");
    }
    for pointer in ["", "/request", "/request/review"] {
        let mut receipt = archive_fixture();
        receipt.pointer_mut(pointer).unwrap()["future"] = json!(null);
        assert!(archive_receipt(&receipt).is_err(), "{pointer}");
    }
    for field in [
        "request",
        "request_sha256",
        "stage",
        "result",
        "error",
        "committed_at",
    ] {
        let mut receipt = archive_fixture();
        receipt.as_object_mut().unwrap().remove(field);
        assert!(archive_receipt(&receipt).is_err(), "{field}");
    }
}

#[test]
fn r264_marker_is_an_exact_contract_not_a_minimum_version_comparison() {
    archive_marker(&json!({})).unwrap();
    archive_marker(&authority_fixture()).unwrap();
    for marker in [
        json!(null),
        json!({}),
        json!({"minimum_reader_release":"r264"}),
    ] {
        assert!(archive_marker(&json!({"archive_storage_contract_v1":marker})).is_err());
    }
    for (key, value) in [
        ("minimum_reader_release", json!("r263")),
        ("minimum_reader_release", json!("r265")),
        ("binary_only_rollback_blocked_to", json!([])),
        ("reason", json!("future")),
        ("future", json!(null)),
    ] {
        let mut marker = marker_fixture();
        marker[key] = value;
        assert!(archive_marker(&json!({"archive_storage_contract_v1":marker})).is_err());
    }
    assert!(archive_marker(&json!({"archive_storage_contract_v2":marker_fixture()})).is_err());
    assert!(super::super::r264_rollback::decode(
        br#"{"archive_storage_contract_v1":{},"archive_storage_contract_v1":{}}"#
    )
    .is_err());
}

fn management_fixture() -> Value {
    json!({"schema":"cutex/agent-management-receipt/v1","action_id":"fixture",
        "request_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "operation":"revoke_operator","project_id":"project.fixture","completed_at":"2026-10-26T00:00:00Z",
        "result":{"kind":"operator_revoked","operator_cutex_session_id":"cutex.fixture",
            "roster":{"grant_revision":1,"operators":[]}},"archiveKind":"soft","restorable":true})
}
fn embedded_authority() -> Value {
    let mut authority = authority_fixture();
    authority["actions"] = json!({"fixture":{"action_id":"fixture",
        "request_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "operation":"revoke_operator","project_id":"project.fixture","caller_cutex_session":"cutex.fixture",
        "phase":"complete","created_at":"2026-10-26T00:00:00Z","updated_at":"2026-10-26T00:00:00Z",
        "response":{"schema":"cutex/agent-management/v1","action_id":"fixture",
            "outcome":{"status":"complete","receipt":management_fixture()}}}});
    authority
}

#[test]
fn management_receipts_use_their_own_enum_and_closed_result_contract() {
    management_receipt(&management_fixture()).unwrap();
    for (pointer, value) in [
        ("/operation", json!("permanent_retire")),
        ("/operation", json!("future")),
        ("/result/kind", json!("future")),
        ("/archiveKind", json!("future")),
        ("/restorable", json!("true")),
    ] {
        let mut receipt = management_fixture();
        *receipt.pointer_mut(pointer).unwrap() = value;
        assert!(management_receipt(&receipt).is_err(), "{pointer}");
    }
    for pointer in ["", "/result"] {
        let mut receipt = management_fixture();
        receipt.pointer_mut(pointer).unwrap()["future"] = json!(null);
        assert!(management_receipt(&receipt).is_err());
    }
}

struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("r265-archive-reader-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn raw(&self, relative: &str, bytes: &[u8]) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    fn write(&self, relative: &str, value: &Value) {
        self.raw(relative, &serde_json::to_vec(value).unwrap());
    }
    fn tree(&self) -> std::collections::BTreeMap<std::path::PathBuf, String> {
        fn walk(
            root: &Path,
            path: &Path,
            tree: &mut std::collections::BTreeMap<std::path::PathBuf, String>,
        ) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                let key = path.strip_prefix(root).unwrap().to_owned();
                if path.is_dir() {
                    tree.insert(key, "directory".into());
                    walk(root, &path, tree);
                } else {
                    tree.insert(
                        key,
                        format!(
                            "{:?}",
                            super::super::r264_rollback::fingerprint(&path).unwrap()
                        ),
                    );
                }
            }
        }
        let mut tree = std::collections::BTreeMap::new();
        walk(&self.0, &self.0, &mut tree);
        tree
    }
    fn check_reader(&self, reader: Reader, allowed: bool) {
        let before = self.tree();
        let result = older_stores::preflight_preserved_stores(&self.0, reader);
        assert_eq!(result.is_ok(), allowed, "{reader:?}: {result:?}");
        assert_eq!(
            self.tree(),
            before,
            "reader modified source bytes/identity/tree"
        );
        let target = match reader {
            Reader::R263 => "r263",
            Reader::R264 => "r264",
        };
        for result in [
            older_stores::preflight(&self.0, target),
            older_stores::prepare(&self.0, &self.0.join("displaced"), target)
                .map(|notes| assert!(notes.is_empty())),
            older_stores::export_hot_stores(&self.0, target, true)
                .map(|notes| assert!(notes.is_empty())),
        ] {
            assert_eq!(result.is_ok(), allowed, "public {target}: {result:?}");
            assert_eq!(self.tree(), before, "public path modified source");
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn r264_archive_inventory_uses_shared_snapshot_without_source_writes() {
    let f = Fixture::new();
    f.write(AUTHORITY, &embedded_authority());
    f.write(
        "cutex-sessions.json",
        &json!({"sessions":{},"agent_archive_receipts":{"fixture":archive_fixture()}}),
    );
    f.check_reader(Reader::R264, true);
    f.check_reader(Reader::R263, false);
    for field in ["agent_archive_actions", "agent_archive_audit"] {
        let mut bad = embedded_authority();
        bad[field]["fixture"]["stage"] = json!("future");
        f.write(AUTHORITY, &bad);
        f.check_reader(Reader::R264, false);
    }
    let mut bad = embedded_authority();
    bad["actions"]["fixture"]["response"]["outcome"]["receipt"]["future"] = json!(true);
    f.write(AUTHORITY, &bad);
    f.check_reader(Reader::R264, false);
    f.write(AUTHORITY, &authority_fixture());
    let mut bad = archive_fixture();
    bad["archiveKind"] = json!("future");
    f.write(
        "cutex-sessions.json",
        &json!({"sessions":{},"agent_archive_receipts":{"fixture":bad}}),
    );
    f.check_reader(Reader::R264, false);
}

#[test]
fn old_archive_contract_keeps_both_reader_results() {
    let f = Fixture::new();
    let mut receipt = archive_fixture();
    receipt["stage"] = json!("committed");
    receipt["request"]["review"]["operation"] = json!("archive");
    receipt.as_object_mut().unwrap().remove("archiveKind");
    receipt.as_object_mut().unwrap().remove("restorable");
    f.write(
        "cutex-sessions.json",
        &json!({"sessions":{},"agent_archive_receipts":{"fixture":receipt}}),
    );
    f.check_reader(Reader::R263, true);
    f.check_reader(Reader::R264, true);
}

#[test]
fn unknown_shared_database_tables_are_not_observation_exemptions() {
    let f = Fixture::new();
    let path = f.0.join("runtime/management-v2/management-state.sqlite3");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute_batch("CREATE TABLE outcome_observations(evidence TEXT)")
        .unwrap();
    drop(db);
    f.check_reader(Reader::R263, false);
    f.check_reader(Reader::R264, false);
}

#[test]
fn removed_observation_namespace_is_unknown_for_both_readers() {
    let f = Fixture::new();
    let path = f.0.join("runtime/management-v2/management-state.sqlite3");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let db = rusqlite::Connection::open(path).unwrap();
    // Negative namespace fixture only: no observation protocol is supported.
    db.execute_batch("CREATE TABLE hot_store_meta (
        store TEXT PRIMARY KEY, format_version INTEGER NOT NULL, generation INTEGER NOT NULL,
        migrated_at TEXT, legacy_sha256 TEXT, verified INTEGER NOT NULL CHECK(verified IN (0,1)));
        CREATE TABLE hot_store_rows (store TEXT NOT NULL,collection TEXT NOT NULL,row_key TEXT NOT NULL,
        value_json BLOB NOT NULL,PRIMARY KEY(store,collection,row_key),
        FOREIGN KEY(store) REFERENCES hot_store_meta(store) ON DELETE CASCADE) WITHOUT ROWID;
        INSERT INTO hot_store_meta VALUES('agent_bus_settlement_evidence',1,1,NULL,NULL,1);").unwrap();
    for reader in [Reader::R263, Reader::R264] {
        let before = f.tree();
        let error = older_stores::preflight_preserved_stores(&f.0, reader).unwrap_err();
        assert!(error.to_string().contains("unknown/orphan hot-store"));
        assert_eq!(f.tree(), before);
    }
    db.execute("UPDATE hot_store_meta SET store='unknown'", [])
        .unwrap();
    for reader in [Reader::R263, Reader::R264] {
        let before = f.tree();
        let error = older_stores::preflight_preserved_stores(&f.0, reader).unwrap_err();
        assert!(error.to_string().contains("unknown/orphan hot-store"));
        assert_eq!(f.tree(), before);
    }
    drop(db);
}

#[test]
fn corrupt_marker_duplicate_json_and_sqlite_never_modify_source() {
    let f = Fixture::new();
    for raw in [
        b"not json".as_slice(),
        b"null",
        br#"{"schema":1,"schema":2}"#,
    ] {
        f.raw(AUTHORITY, raw);
        f.check_reader(Reader::R264, false);
    }
    let mut bad = authority_fixture();
    bad["archive_storage_contract_v1"]["minimum_reader_release"] = json!("r265");
    f.write(AUTHORITY, &bad);
    f.check_reader(Reader::R264, false);
    f.write(AUTHORITY, &authority_fixture());
    f.raw("cutex-sessions.sqlite3", b"corrupt database");
    f.check_reader(Reader::R264, false);
}

fn session_database(f: &Fixture) -> rusqlite::Connection {
    let db = rusqlite::Connection::open(f.0.join("cutex-sessions.sqlite3")).unwrap();
    // Frozen 8dba schema, not a store open/import using current writer code.
    db.execute_batch("PRAGMA user_version=1;
        CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL) WITHOUT ROWID;
        CREATE TABLE sessions(key TEXT PRIMARY KEY,cutex_session_id TEXT NOT NULL,codex_session_id TEXT,
            formal_agent_name TEXT,archive_state TEXT NOT NULL,rev INTEGER NOT NULL,record_json TEXT NOT NULL);
        CREATE INDEX sessions_rev ON sessions(rev);
        CREATE INDEX sessions_codex ON sessions(codex_session_id) WHERE codex_session_id IS NOT NULL;
        CREATE INDEX sessions_name ON sessions(formal_agent_name) WHERE formal_agent_name IS NOT NULL;
        CREATE TABLE receipts(map TEXT NOT NULL,key TEXT NOT NULL,rev INTEGER NOT NULL,json TEXT NOT NULL,PRIMARY KEY(map,key));
        CREATE INDEX receipts_rev ON receipts(rev);
        INSERT INTO meta VALUES('schema_version','1'),('store_revision','1'),
            ('created_at','2026-10-26T00:00:00Z');
        PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;").unwrap();
    db
}

#[test]
fn r264_wal_archive_receipts_are_validated_in_the_disposable_copy() {
    let f = Fixture::new();
    let db = session_database(&f);
    db.execute(
        "INSERT INTO receipts VALUES('agent_archive','fixture',1,?1)",
        [archive_fixture().to_string()],
    )
    .unwrap();
    f.write(
        "cutex-sessions.json",
        &json!({"cutexSessionStore":"migrated-to-sqlite","database":"cutex-sessions.sqlite3"}),
    );
    f.check_reader(Reader::R264, true);
    f.check_reader(Reader::R263, false);
    let mut bad = archive_fixture();
    bad["request"]["review"]["operation"] = json!("future");
    db.execute("UPDATE receipts SET json=?1", [bad.to_string()])
        .unwrap();
    f.check_reader(Reader::R264, false);
    db.execute("UPDATE receipts SET map='future',json='{}'", [])
        .unwrap();
    f.check_reader(Reader::R264, false);
    drop(db);
}

fn historical_archive_fixture() -> Value {
    let mut receipt = archive_fixture();
    receipt["stage"] = json!("committed");
    receipt["request"]["review"]["operation"] = json!("archive");
    receipt.as_object_mut().unwrap().remove("archiveKind");
    receipt.as_object_mut().unwrap().remove("restorable");
    receipt
}

#[test]
fn historical_review_digests_survive_all_preserved_receipt_locations() {
    for fields in [
        vec!["durable_sha256"],
        vec!["authority_sha256"],
        vec!["durable_sha256", "authority_sha256"],
    ] {
        let mut receipt = historical_archive_fixture();
        for field in fields {
            receipt["request"]["review"][field] = json!("0123456789abcdef".repeat(4));
        }
        for location in [
            "agent_archive_actions",
            "agent_archive_audit",
            "legacy",
            "sqlite",
        ] {
            let f = Fixture::new();
            // Keep the writer alive so the receipt is committed only in WAL.
            let db = if location == "sqlite" {
                let db = session_database(&f);
                db.execute(
                    "INSERT INTO receipts VALUES('agent_archive','fixture',1,?1)",
                    [receipt.to_string()],
                )
                .unwrap();
                assert!(
                    std::fs::metadata(f.0.join("cutex-sessions.sqlite3-wal"))
                        .unwrap()
                        .len()
                        > 0
                );
                Some(db)
            } else {
                let state = if location == "legacy" {
                    json!({"sessions":{},"agent_archive_receipts":{"fixture":receipt}})
                } else {
                    json!({"schema":"cutex/agent-management-store/v1","store_revision":0,
                        "projects":{},"agents":{},"actions":{},"authority_receipts":{},
                        "failure_events":{},location:{"fixture":receipt}})
                };
                f.write(
                    if location == "legacy" {
                        "cutex-sessions.json"
                    } else {
                        AUTHORITY
                    },
                    &state,
                );
                None
            };
            check_both(&f, true);
            drop(db);
        }
    }
}

#[test]
fn r264_historical_digest_values_are_validated_without_admitting_extensions() {
    let f = Fixture::new();
    for field in ["durable_sha256", "authority_sha256"] {
        for value in [
            json!(null),
            json!(1),
            json!({}),
            json!("a".repeat(63)),
            json!("a".repeat(65)),
            json!("A".repeat(64)),
            json!("g".repeat(64)),
        ] {
            let mut receipt = historical_archive_fixture();
            receipt["request"]["review"][field] = value;
            f.write(
                "cutex-sessions.json",
                &json!({"sessions":{},"agent_archive_receipts":{"fixture":receipt}}),
            );
            f.check_reader(Reader::R264, false);
        }
    }
    let mut receipt = historical_archive_fixture();
    receipt["request"]["review"]["durable_sha256"] = json!("a".repeat(64));
    receipt["request"]["review"]["future_digest"] = json!("a".repeat(64));
    f.write(
        "cutex-sessions.json",
        &json!({"sessions":{},"agent_archive_receipts":{"fixture":receipt}}),
    );
    f.check_reader(Reader::R264, false);
}

#[test]
fn both_readers_refuse_unknown_archive_marker_versions_in_each_json_store() {
    for location in [AUTHORITY, "cutex-sessions.json"] {
        let f = Fixture::new();
        let old = if location == AUTHORITY {
            json!({"schema":"cutex/agent-management-store/v1","store_revision":0,
                "projects":{},"agents":{},"actions":{},"authority_receipts":{},"failure_events":{}})
        } else {
            json!({"sessions":{}})
        };
        f.write(location, &old);
        check_both(&f, true);
        for key in [
            "archive_storage_contract_v2",
            "archive_storage_contract_future",
        ] {
            let mut state = old.clone();
            state[key] = json!({"minimum_reader_release":"r265"});
            f.write(location, &state);
            check_both(&f, false);
        }
        let mut state = old;
        state["archive_storage_contract_v1"] = marker_fixture();
        f.write(location, &state);
        f.check_reader(Reader::R263, false);
        f.check_reader(Reader::R264, true);
    }
}

#[test]
fn both_readers_validate_session_store_and_row_revisions_in_wal() {
    let f = Fixture::new();
    let db = session_database(&f);
    let record = crate::session::model::CutexSessionRecord::new_at(
        "cutex.fixture".into(),
        Some("native-fixture".into()),
        "fixture-host".into(),
        f.0.to_string_lossy().into_owned(),
        None,
        "2026-10-26T00:00:00Z".into(),
    )
    .unwrap();
    db.execute("INSERT INTO sessions VALUES('cutex.fixture','cutex.fixture','native-fixture',NULL,'active',1,?1)",
        [serde_json::to_string(&record).unwrap()]).unwrap();
    db.execute(
        "INSERT INTO receipts VALUES('agent_archive','fixture',1,?1)",
        [historical_archive_fixture().to_string()],
    )
    .unwrap();
    // A clean checkpoint makes every following mutation a WAL-only change.
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    check_both(&f, true);
    for sql in [
        "DELETE FROM meta WHERE key='store_revision'",
        "UPDATE meta SET value='bad' WHERE key='store_revision'",
        "UPDATE meta SET value='-1' WHERE key='store_revision'",
        "UPDATE meta SET value='18446744073709551616' WHERE key='store_revision'",
        "UPDATE meta SET value=x'31' WHERE key='store_revision'",
        "UPDATE sessions SET rev=-1",
        "UPDATE receipts SET rev=-1",
        "UPDATE sessions SET rev='bad'",
        "UPDATE receipts SET rev=1.5",
    ] {
        db.execute_batch(sql).unwrap();
        assert!(
            std::fs::metadata(f.0.join("cutex-sessions.sqlite3-wal"))
                .unwrap()
                .len()
                > 0
        );
        check_both(&f, false);
        db.execute_batch(
            "INSERT OR REPLACE INTO meta VALUES('store_revision','1');
            UPDATE sessions SET rev=1; UPDATE receipts SET rev=1;",
        )
        .unwrap();
        check_both(&f, true);
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    }
    // Match the actual predecessor's u64 parser, not a new i64/canonical-text rule.
    for revision in ["0", "01", "18446744073709551615"] {
        db.execute(
            "UPDATE meta SET value=?1 WHERE key='store_revision'",
            [revision],
        )
        .unwrap();
        check_both(&f, true);
    }
    drop(db);
}

#[test]
fn both_readers_refuse_missing_session_metadata_and_revision_columns() {
    for sql in [
        "DROP TABLE meta",
        "ALTER TABLE meta RENAME COLUMN value TO unknown_value",
        "ALTER TABLE sessions RENAME COLUMN rev TO unknown_rev",
        "ALTER TABLE receipts RENAME COLUMN rev TO unknown_rev",
    ] {
        let f = Fixture::new();
        let db = session_database(&f);
        check_both(&f, true);
        db.execute_batch(sql).unwrap();
        // Even empty tables must have the predecessor's required columns.
        check_both(&f, false);
        drop(db);
    }
}

const BUS_JSON: &str = "runtime/management-v2/agent-bus-message-state.json";
const MANAGEMENT_DB: &str = "runtime/management-v2/management-state.sqlite3";
const HOT_DDL: &str = "CREATE TABLE hot_store_meta (
    store TEXT PRIMARY KEY,format_version INTEGER NOT NULL,generation INTEGER NOT NULL,
    migrated_at TEXT,legacy_sha256 TEXT,verified INTEGER NOT NULL);
    CREATE TABLE hot_store_rows(store TEXT NOT NULL,collection TEXT NOT NULL,row_key TEXT NOT NULL,
    value_json BLOB NOT NULL,PRIMARY KEY(store,collection,row_key)) WITHOUT ROWID;";

fn check_both(f: &Fixture, allowed: bool) {
    for reader in [Reader::R263, Reader::R264] {
        f.check_reader(reader, allowed);
    }
}

#[test]
fn both_previous_readers_accept_v7_and_reject_unsupported_legacy_bus_versions() {
    let f = Fixture::new();
    for version in [
        json!(7),
        json!(8),
        json!(99),
        json!("7"),
        json!(-1),
        json!(null),
    ] {
        f.write(BUS_JSON, &json!({"version":version,"messages":{}}));
        check_both(&f, version == json!(7));
    }
}

#[test]
fn both_previous_readers_validate_hot_v7_wal_metadata_and_unknown_rows() {
    let f = Fixture::new();
    let path = f.0.join(MANAGEMENT_DB);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(HOT_DDL).unwrap();
    db.execute_batch(
        "INSERT INTO hot_store_meta VALUES('agent_bus',1,1,NULL,NULL,1);
        INSERT INTO hot_store_rows VALUES('agent_bus','state','version',CAST('7' AS BLOB));",
    )
    .unwrap();
    check_both(&f, true);
    db.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
        UPDATE hot_store_rows SET value_json=CAST('8' AS BLOB);",
    )
    .unwrap();
    assert!(
        std::fs::metadata(format!("{}-wal", path.display()))
            .unwrap()
            .len()
            > 0
    );
    check_both(&f, false);
    db.execute_batch("UPDATE hot_store_rows SET value_json=CAST('7' AS BLOB)")
        .unwrap();
    check_both(&f, true);
    for sql in [
        "UPDATE hot_store_meta SET verified=0",
        "UPDATE hot_store_meta SET format_version=99",
        "UPDATE hot_store_meta SET generation=-1",
        "UPDATE hot_store_meta SET verified=2",
        "UPDATE hot_store_rows SET collection='unknown'",
        "UPDATE hot_store_rows SET store='orphan'",
    ] {
        db.execute_batch(sql).unwrap();
        check_both(&f, false);
        db.execute_batch(
            "UPDATE hot_store_meta SET verified=1,format_version=1,generation=1;
            UPDATE hot_store_rows SET collection='state',store='agent_bus';",
        )
        .unwrap();
    }
    drop(db);
}

#[test]
fn both_previous_readers_reject_orphan_sqlite_sidecars_without_source_writes() {
    for database in [
        MANAGEMENT_DB,
        "runtime/management-v2/pending-stops.sqlite",
        "runtime/management-v2/integration-event-dedupe-v2.sqlite",
        "cutex-sessions.sqlite3",
    ] {
        for suffix in ["-wal", "-shm", "-journal"] {
            let f = Fixture::new();
            f.raw(&format!("{database}{suffix}"), b"orphan");
            check_both(&f, false);
        }
    }
}
