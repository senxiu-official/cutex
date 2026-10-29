// Both public preflight and the real CLI implementation must decide identically.
#[cfg(windows)]
#[test]
fn r263_native_retry_denied_oversize_and_junction_refuse_without_writes() {
    use std::os::windows::fs::OpenOptionsExt;
    let f = R263Fixture::new();
    f.write(RETRY_GUARD, &inert_retry());
    let before = f.tree();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(f.0.join(RETRY_GUARD))
        .unwrap();
    assert!(cutex::upgrade::older_stores::preflight(&f.0, "r263").is_err());
    assert!(super::rollback_export_at(&f.0, "r263", true).is_err());
    drop(held);
    assert_eq!(before, f.tree());
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(f.0.join(RETRY_GUARD))
        .unwrap();
    file.set_len(64 * 1024 * 1024 + 1).unwrap();
    drop(file);
    assert!(cutex::upgrade::older_stores::preflight(&f.0, "r263").is_err());
    assert!(super::rollback_export_at(&f.0, "r263", true).is_err());
    assert_eq!(
        std::fs::metadata(f.0.join(RETRY_GUARD)).unwrap().len(),
        64 * 1024 * 1024 + 1
    );
    f.write(RETRY_GUARD, &inert_retry());
    let original = f.0.join("runtime/model-overload-retry");
    let target = f.0.join("private-target");
    std::fs::rename(&original, &target).unwrap();
    let created = std::process::Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(original.to_string_lossy().replace('/', "\\"))
        .arg(target.to_string_lossy().replace('/', "\\"))
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "private junction fixture creation failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let saved = std::fs::read(target.join("state-v1.json")).unwrap();
    let preflight = cutex::upgrade::older_stores::preflight(&f.0, "r263");
    let cli = super::rollback_export_at(&f.0, "r263", true);
    // Remove only the fixture junction itself before the RAII tree cleanup.
    std::fs::remove_dir(&original).unwrap();
    assert!(preflight.is_err());
    assert!(cli.is_err());
    assert_eq!(std::fs::read(target.join("state-v1.json")).unwrap(), saved);
    assert!(!f.0.join("rollback-export").exists());
}

struct R263Fixture(std::path::PathBuf);
impl R263Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("r263-guard-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn write(&self, path: &str, value: &serde_json::Value) {
        self.raw(path, &serde_json::to_vec(value).unwrap());
    }
    fn raw(&self, path: &str, bytes: &[u8]) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    fn tree(
        &self,
    ) -> std::collections::BTreeMap<std::path::PathBuf, (Vec<u8>, Option<(u64, u64)>)> {
        fn walk(
            root: &std::path::Path,
            path: &std::path::Path,
            out: &mut std::collections::BTreeMap<std::path::PathBuf, (Vec<u8>, Option<(u64, u64)>)>,
        ) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                let key = path.strip_prefix(root).unwrap().to_owned();
                if path.is_dir() {
                    out.insert(key, (vec![], None));
                    walk(root, &path, out);
                } else {
                    #[cfg(windows)]
                    let identity = Some(
                        cutex::platform::private_fs::identity(&std::fs::File::open(&path).unwrap())
                            .unwrap()
                            .publication_key(),
                    );
                    #[cfg(unix)]
                    let identity = {
                        use std::os::unix::fs::MetadataExt;
                        let m = path.metadata().unwrap();
                        Some((m.dev(), m.ino()))
                    };
                    #[cfg(not(any(unix, windows)))]
                    let identity = None;
                    out.insert(key, (std::fs::read(path).unwrap(), identity));
                }
            }
        }
        let mut out = std::collections::BTreeMap::new();
        walk(&self.0, &self.0, &mut out);
        out
    }
    fn check(&self, allowed: bool) {
        let before = self.tree();
        let preflight = cutex::upgrade::older_stores::preflight(&self.0, "r263");
        assert_eq!(preflight.is_ok(), allowed, "preflight: {preflight:?}");
        assert_eq!(self.tree(), before, "preflight mutated source");
        let result = super::rollback_export_at(&self.0, "r263", true);
        assert_eq!(result.is_ok(), allowed, "CLI: {result:?}");
        if allowed {
            assert_eq!(
                result.unwrap(),
                json!({"target":"r263","daemonStopped":true,"exports":[],"displaced":null,"backup":null,"hotStoresPreserved":true})
            );
        }
        assert_eq!(self.tree(), before, "CLI mutated source");
    }
}
impl Drop for R263Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
const RETRY_GUARD: &str = "runtime/model-overload-retry/state-v1.json";
const AUTHORITY_GUARD: &str = "runtime/agent-management/v1/agent-management-v1.json";
const PENDING_GUARD: &str = "runtime/management-v2/pending-stops.sqlite";
const PENDING_SCHEMA:&str="CREATE TABLE pending_stop(id INTEGER PRIMARY KEY, session TEXT NOT NULL, generation TEXT NOT NULL, receipt_path TEXT NOT NULL, host TEXT NOT NULL, message TEXT NOT NULL, fallback INTEGER NOT NULL, due INTEGER NOT NULL DEFAULT 0, UNIQUE(session,generation)); CREATE INDEX pending_stop_due ON pending_stop(due,id);";
fn inert_retry() -> serde_json::Value {
    json!({"version":1,"sessions":{"cutex.fixture":{"revision":1,"plan":null,"settlement":null,"last":{"opaque":[1,true,null]},"seen":["event"],"blockedGeneration":7}}})
}
fn old_archive_receipt() -> serde_json::Value {
    json!({"request":{"action_id":"fixture","review":{"cutex_session_id":"cutex.fixture","formal_name":"Fixture","operation":"archive","revision":1,"runtime_generation":7}},"request_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stage":"committed","result":null,"error":null,"committed_at":"2026-10-26T00:00:00Z"})
}

#[test]
fn r263_native_retry_obligations_even_disabled_and_inert_history() {
    let f = R263Fixture::new();
    f.check(true);
    f.raw("runtime/model-overload-retry/state-v1.lock", b"");
    f.check(true);
    f.write(RETRY_GUARD, &inert_retry());
    f.check(true);
    for disabled in [false, true] {
        if disabled {
            f.raw(
                "runtime/model-overload-retry/state-v1.disabled",
                b"disabled",
            );
        }
        f.write(RETRY_GUARD, &inert_retry());
        f.check(true);
        for field in ["plan", "settlement"] {
            for phase in [
                "pending",
                "firing",
                "running",
                "uncertain",
                "binding",
                "unknown",
            ] {
                let mut state = inert_retry();
                state["sessions"]["cutex.fixture"][field] = json!({"phase":phase});
                f.write(RETRY_GUARD, &state);
                f.check(false);
            }
        }
    }
    std::fs::remove_file(f.0.join(RETRY_GUARD)).unwrap();
    f.check(false);
}

#[test]
fn r263_native_retry_strict_schema_and_partial_files() {
    let f = R263Fixture::new();
    for raw in [
        b"{}".as_slice(),
        b"{\"version\":1,\"version\":1,\"sessions\":{}}",
        b"{\"version\":1,\"sessions\":{},\"x\":null}",
        b"{\"version\":1,\"sessions\":{\"a\":{}}}",
        b"{\"version\":1,\"sessions\":{},\"x\":1e999}",
        b"\xff",
        b"null",
    ] {
        f.raw(RETRY_GUARD, raw);
        f.check(false);
    }
    for (pointer, value) in [
        ("/version", json!(0)),
        ("/version", json!(true)),
        ("/version", json!(1.0)),
        ("/sessions/cutex.fixture/revision", json!(-1)),
        ("/sessions/cutex.fixture/seen", json!([1])),
        ("/sessions/cutex.fixture/blockedGeneration", json!("7")),
        ("/sessions/cutex.fixture/plan", json!(false)),
        ("/sessions/cutex.fixture/settlement", json!([])),
    ] {
        let mut state = inert_retry();
        *state.pointer_mut(pointer).unwrap() = value;
        f.write(RETRY_GUARD, &state);
        f.check(false);
    }
    f.write(RETRY_GUARD, &inert_retry());
    f.raw("runtime/model-overload-retry/incomplete.tmp", b"partial");
    f.check(false);
}

#[test]
fn r263_native_pending_exact_empty_schema_and_all_debts() {
    let f = R263Fixture::new();
    let path = f.0.join(PENDING_GUARD);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(PENDING_SCHEMA).unwrap();
    }
    f.check(true);
    for payload in ["Closing", "Offline", "Failed"] {
        {
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute(
                "INSERT INTO pending_stop VALUES(1,'s','9','r','h',?1,1,999999999999)",
                [payload],
            )
            .unwrap();
        }
        f.check(false);
        {
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute("DELETE FROM pending_stop", []).unwrap();
        }
    }
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE INDEX unknown_index ON pending_stop(host)")
            .unwrap();
    }
    f.check(false);
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("DROP INDEX unknown_index; PRAGMA user_version=1")
            .unwrap();
    }
    f.check(false);
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA user_version=0; PRAGMA application_id=1")
            .unwrap();
    }
    f.check(false);
}

#[test]
fn r263_native_pending_wal_debt_and_orphan_sidecars() {
    let f = R263Fixture::new();
    let path = f.0.join(PENDING_GUARD);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(PENDING_SCHEMA).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; INSERT INTO pending_stop VALUES(1,'s','9','r','h','Offline',0,0)").unwrap();
    assert!(
        std::fs::metadata(format!("{}-wal", path.display()))
            .unwrap()
            .len()
            > 0
    );
    f.check(false);
    drop(db);
    std::fs::remove_file(&path).unwrap();
    f.raw(&format!("{PENDING_GUARD}-wal"), b"orphan");
    f.check(false);
}

#[test]
fn r263_native_archive_marker_old_receipts_and_additive_authority() {
    let f = R263Fixture::new();
    let mut authority = json!({"schema":"cutex/agent-management-store/v1","store_revision":0,"projects":{},"agents":{},"actions":{},"authority_receipts":{},"failure_events":{}});
    authority["tool_action_receipts_v1"] = json!({"opaque":"preserve"});
    authority["agent_archive_actions"] = json!({"fixture":old_archive_receipt()});
    authority["agent_archive_audit"] = json!({"fixture":old_archive_receipt()});
    f.write(AUTHORITY_GUARD, &authority);
    f.check(true);
    for marker in [
        serde_json::Value::Null,
        json!({}),
        json!({"minimum_reader_release":"r263"}),
    ] {
        let mut bad = authority.clone();
        bad["archive_storage_contract_v1"] = marker;
        f.write(AUTHORITY_GUARD, &bad);
        f.check(false);
    }
    for field in ["agent_archive_actions", "agent_archive_audit"] {
        for (key, value) in [
            ("archiveKind", json!("soft")),
            ("restorable", json!(null)),
            ("stage", json!("permanent_retirement_committed")),
        ] {
            let mut bad = authority.clone();
            bad[field]["fixture"][key] = value;
            f.write(AUTHORITY_GUARD, &bad);
            f.check(false);
        }
    }
    f.write(AUTHORITY_GUARD, &authority);
    f.write(
        "cutex-sessions.json",
        &json!({"sessions":{},"agent_archive_receipts":{"fixture":old_archive_receipt()}}),
    );
    f.check(true);
    let mut bad = old_archive_receipt();
    bad["request"]["review"]["operation"] = json!("permanent_retire");
    f.write(
        "cutex-sessions.json",
        &json!({"sessions":{},"agent_archive_receipts":{"fixture":bad}}),
    );
    f.check(false);
}

#[test]
fn r263_native_archive_wal_receipts_and_unknown_maps() {
    let f = R263Fixture::new();
    let path = f.0.join("cutex-sessions.sqlite3");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA user_version=1; CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO meta VALUES('store_revision','1'); CREATE TABLE sessions(key TEXT,rev INTEGER,record_json TEXT); CREATE TABLE receipts(map TEXT,key TEXT,rev INTEGER,json TEXT); PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;").unwrap();
    db.execute(
        "INSERT INTO receipts VALUES('agent_archive','fixture',1,?1)",
        [old_archive_receipt().to_string()],
    )
    .unwrap();
    f.check(true);
    let mut bad = old_archive_receipt();
    bad["restorable"] = json!(true);
    db.execute("UPDATE receipts SET json=?1", [bad.to_string()])
        .unwrap();
    f.check(false);
    db.execute("UPDATE receipts SET map='unknown',json='{}'", [])
        .unwrap();
    f.check(false);
    drop(db);
}

#[test]
fn r263_native_nested_management_receipt_old_and_new_fields() {
    let f = R263Fixture::new();
    let receipt = json!({"schema":"cutex/agent-management-receipt/v1","action_id":"fixture","request_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","operation":"revoke_operator","project_id":"project.fixture","completed_at":"2026-10-26T00:00:00Z","result":{"kind":"operator_revoked","operator_cutex_session_id":"cutex.fixture","roster":{"grant_revision":1,"operators":[]}}});
    let mut authority = json!({"schema":"cutex/agent-management-store/v1","store_revision":0,"projects":{},"agents":{},"authority_receipts":{},"failure_events":{},"actions":{"fixture":{"action_id":"fixture","request_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","operation":"revoke_operator","project_id":"project.fixture","caller_cutex_session":"cutex.fixture","phase":"complete","created_at":"2026-10-26T00:00:00Z","updated_at":"2026-10-26T00:00:00Z","response":{"schema":"cutex/agent-management/v1","action_id":"fixture","outcome":{"status":"complete","receipt":receipt}}}}});
    f.write(AUTHORITY_GUARD, &authority);
    f.check(true);
    let pointer = "/actions/fixture/response/outcome/receipt";
    for (field, value) in [
        ("archiveKind", json!("soft")),
        ("restorable", json!(null)),
        ("future", json!(false)),
    ] {
        let mut bad = authority.clone();
        bad.pointer_mut(pointer).unwrap()[field] = value;
        f.write(AUTHORITY_GUARD, &bad);
        f.check(false);
    }
    authority["actions"]["fixture"]["response"]["outcome"] = json!({"status":"future"});
    f.write(AUTHORITY_GUARD, &authority);
    f.check(false);
}

#[test]
fn r263_native_unknown_hot_schema_and_offline_assertion() {
    let f = R263Fixture::new();
    assert!(super::rollback_export_at(&f.0, "r263", false).is_err());
    assert!(super::rollback_export_at(&f.0, "r0263", true).is_err());
    assert!(super::rollback_export_at(&f.0, "r265", true).is_err());
    assert!(f.tree().is_empty());
    let path = f.0.join("runtime/management-v2/management-state.sqlite3");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    {
        let db = rusqlite::Connection::open(path).unwrap();
        db.execute_batch("CREATE TABLE unknown(x)").unwrap();
    }
    f.check(false);
}
