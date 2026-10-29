//! Offline, backed-up conversion of peer asks for a pre-peers daemon.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};
use fs2::FileExt;
use serde::Serialize;
use serde_json::{json, Value};

use crate::asks::model::{ExecutionState, LeaseHolder};
use crate::asks::store::{AskState, RuntimeCommand};
use crate::config::atomic::{write_private_bytes_atomic, write_private_pretty_json_atomic};

fn lock_file(path: &Path) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn migration_lock(runtime: &Path) -> anyhow::Result<File> {
    std::fs::create_dir_all(runtime)?;
    secure_directory(runtime)?;
    lock_file(&runtime.join("asks-migration.lock"))
}

fn secure_directory(path: &Path) -> anyhow::Result<()> {
    #[cfg(windows)]
    crate::platform::private_fs::secure_directory(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            std::fs::symlink_metadata(path)?.file_type().is_dir(),
            "private directory must not be a symlink"
        );
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Held for the lifetime of either daemon, before it starts state workers.
pub fn daemon_guard() -> anyhow::Result<File> {
    let guard = migration_lock(&crate::config::paths::runtime_dir()?)?;
    FileExt::try_lock_shared(&guard)
        .context("ask-store migration is in progress; daemon startup refused")?;
    Ok(guard)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub ask_id: String,
    pub direction: String,
    pub before: String,
    pub after: String,
    pub reason: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub export_dir: PathBuf,
    pub backup: PathBuf,
    pub store: PathBuf,
    pub changes: Vec<Change>,
}

/// Does not contact or start a daemon. An active listener makes bind fail.
pub fn export_and_strip(out: Option<&Path>) -> anyhow::Result<Report> {
    let runtime = crate::config::paths::runtime_dir()?;
    let config = crate::config::store::load_codez_config_checked()?;
    let ports = [
        crate::management::service::management_port(&config),
        crate::agent_bus::service::agent_bus_port(&config),
    ];
    export_at(&runtime, &ports, out)
}

fn export_at(runtime: &Path, ports: &[u16], out: Option<&Path>) -> anyhow::Result<Report> {
    let guard = migration_lock(runtime)?;
    guard
        .try_lock_exclusive()
        .context("stop both management and agent-bus daemons before peers export-and-strip")?;
    // Older daemons do not take the migration guard. Hold their bus lock and
    // reserve configured loopback listeners until the replacement is durable.
    let bus_dir = runtime.join("task-service");
    std::fs::create_dir_all(&bus_dir)?;
    let bus_lock = lock_file(&bus_dir.join("agent-bus-host.lock"))?;
    bus_lock
        .try_lock_exclusive()
        .context("agent-bus daemon is running")?;
    let mut listeners = Vec::new();
    for port in ports.iter().copied().collect::<BTreeSet<_>>() {
        listeners.push(
            TcpListener::bind((Ipv4Addr::LOCALHOST, port)).with_context(|| {
                format!("port {port} is in use; stop both daemons before peers export-and-strip")
            })?,
        );
    }
    let asks_dir = runtime.join("asks");
    std::fs::create_dir_all(&asks_dir)?;
    let asks_lock = lock_file(&asks_dir.join("asks-v1.lock"))?;
    asks_lock
        .try_lock_exclusive()
        .context("ask store is in use")?;
    let database = runtime
        .join("management-v2")
        .join(crate::management::v2::hot_store::DATABASE_FILE);
    let store_path = asks_dir.join("asks-v1.json");
    let loaded = crate::management::v2::hot_store::verified_rows(&database, "asks")?;
    let (original, original_state, generation) = if let Some(loaded) = loaded {
        let state = crate::asks::store::ask_state_from_rows(&loaded.rows)?;
        (
            serde_json::to_vec_pretty(&state)?,
            Some(state),
            loaded.generation,
        )
    } else if crate::management::v2::hot_store::has_store(&database, "asks")? {
        // rollback-export made JSON authoritative; re-import it before the
        // second conversion so old-binary changes cannot be lost.
        let loaded = crate::management::v2::hot_store::load_or_migrate(
            &database,
            "asks",
            &store_path,
            |bytes| {
                let state: AskState =
                    serde_json::from_slice(bytes).context("parse handed-off ask store")?;
                crate::asks::store::ask_state_rows(&state)
            },
            |rows| crate::asks::store::ask_state_from_rows(rows).map(|_| ()),
        )?;
        let state = crate::asks::store::ask_state_from_rows(&loaded.rows)?;
        (
            serde_json::to_vec_pretty(&state)?,
            Some(state),
            loaded.generation,
        )
    } else {
        // A genuinely pre-SQLite peer store can contain fields that current
        // AskState deliberately no longer parses. `convert` validates the
        // stripped representation before the first SQLite commit below.
        (
            std::fs::read(&store_path).context("read original ask store")?,
            None,
            0,
        )
    };
    let value: Value = serde_json::from_slice(&original).context("parse original ask store")?;
    ensure_peer_targets_stopped(&value, &crate::session::store::load_cutex_session_store()?)?;
    let now = chrono::Utc::now();
    let (stripped, changes, exported) = convert(value.clone(), now)?;

    let out = out.map(Path::to_path_buf).unwrap_or_else(|| {
        runtime.join("peer-exports").join(format!(
            "{}-{}",
            now.format("%Y%m%dT%H%M%SZ"),
            uuid::Uuid::new_v4().simple()
        ))
    });
    ensure!(
        !out.exists(),
        "export directory already exists; choose a new --out directory"
    );
    std::fs::create_dir_all(&out)?;
    secure_directory(&out)?;
    let backup = out.join("asks-v1.original.json");
    write_private_bytes_atomic(&backup, &original)?;
    // A JSON backup alone is incomplete: preserve every referenced blob too.
    if let Some(artifacts) = value.get("artifacts").and_then(Value::as_object) {
        for id in artifacts.keys() {
            let digest = id
                .strip_prefix("art-")
                .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                .context("invalid artifact id; original store left unchanged")?;
            let name = format!("{digest}.blob");
            let source = asks_dir.join("artifacts").join(&name);
            ensure!(
                std::fs::symlink_metadata(&source)?.file_type().is_file(),
                "artifact must be a regular file; original store left unchanged"
            );
            write_private_bytes_atomic(&out.join("artifacts").join(name), &std::fs::read(source)?)?;
        }
    }
    write_private_pretty_json_atomic(&out.join("peer-records.json"), &exported, "peer export")?;
    let report = Report {
        export_dir: out.clone(),
        backup,
        store: store_path.clone(),
        changes,
    };
    write_private_pretty_json_atomic(&out.join("manifest.json"), &report, "strip manifest")?;
    let mut list = String::from(
        "Peer rollback export\n\nThe original store and artifact blobs are backed up here.\n\
         peerStripped is recorded as text; legacy enum values are interrupted/stopped.\n\
         Remote identity, routing, synchronization, late answers and hops are removed.\n\
         Incoming peer requesters become owner records; original provenance is in peer-records.json.\n\n",
    );
    for change in &report.changes {
        use std::fmt::Write;
        writeln!(
            list,
            "{} [{}]: {} -> {} ({})",
            change.ask_id, change.direction, change.before, change.after, change.reason
        )?;
    }
    write_private_bytes_atomic(&out.join("README.txt"), list.as_bytes())?;
    // Last fallible state-changing step: every backup/export is durable first.
    // Commit the converted state to SQLite, then replace the legacy rollback
    // view from that verified commit.
    let stripped_state: crate::asks::store::AskState = serde_json::from_value(stripped)?;
    let changes = if let Some(original_state) = original_state.as_ref() {
        crate::asks::store::ask_state_changes(original_state, &stripped_state)?
    } else {
        crate::asks::store::ask_state_rows(&stripped_state)?
            .into_iter()
            .map(
                |((collection, key), value)| crate::management::v2::hot_store::RowChange::Put {
                    collection,
                    key,
                    value,
                },
            )
            .collect()
    };
    let generation =
        crate::management::v2::hot_store::commit(&database, "asks", generation, changes)
            .context("commit stripped ask store")?;
    write_private_pretty_json_atomic(&store_path, &stripped_state, "stripped asks")?;
    let checked: crate::asks::store::AskState =
        serde_json::from_slice(&std::fs::read(&store_path)?)?;
    anyhow::ensure!(
        checked == stripped_state,
        "stripped ask JSON verification failed"
    );
    crate::management::v2::hot_store::handoff_to_legacy(&database, "asks", generation)
        .context("hand off stripped ask store to the legacy JSON reader")?;
    Ok(report)
}

const PEER_FIELDS: &[&str] = &[
    "remote",
    "peerInbound",
    "hops",
    "lateAnswer",
    "resultUndeliverable",
];

fn ensure_peer_targets_stopped(
    value: &Value,
    sessions: &crate::session::model::CutexSessionStore,
) -> anyhow::Result<()> {
    let Some(asks) = value.get("asks").and_then(Value::as_object) else {
        return Ok(());
    };
    for ask in asks.values().filter(|ask| {
        ask.get("peerInbound").is_some_and(|v| !v.is_null())
            || ask.pointer("/requester/kind").and_then(Value::as_str) == Some("peer")
    }) {
        let target = ask
            .get("targetCutexSessionId")
            .and_then(Value::as_str)
            .context("missing inbound target")?;
        let active_attempt =
            ask.get("attempts")
                .and_then(Value::as_array)
                .is_some_and(|attempts| {
                    attempts.iter().any(|a| {
                        matches!(
                            a.get("state").and_then(Value::as_str),
                            Some("submitting" | "admitted" | "running")
                        )
                    })
                });
        let Some(record) = sessions.sessions.get(target) else {
            ensure!(
                !active_attempt,
                "cannot prove native target {target} stopped; restore its session metadata first"
            );
            continue;
        };
        ensure!(
            record.pending_launch_id.is_none() && record.app_server_launch_claim_id.is_none(),
            "target {target} has an unresolved native launch; resolve it before rollback"
        );
        let fence = crate::session::offline_reconciliation::durable_offline_occurrence(record);
        for pid in crate::session::offline_reconciliation::occurrence_pids(&fence) {
            ensure!(pid != 0 && pid <= i32::MAX as u32 && !crate::platform::process::process_is_running(pid),
                "native target {target} has a live or unverifiable PID {pid}; stop its agent before stopping the daemons");
        }
        ensure!(
            !active_attempt
                || record.app_server_runtime.is_some()
                || record.runtime_pid.is_some()
                || record.alden_pid.is_some()
                || record.current_runtime_agent_id.is_none(),
            "cannot prove native target {target} stopped"
        );
    }
    Ok(())
}

fn convert(
    mut value: Value,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<(Value, Vec<Change>, Value)> {
    let at = now.to_rfc3339();
    let mut changes = Vec::new();
    let mut exported = serde_json::Map::new();
    if let Some(receipts) = value
        .as_object_mut()
        .and_then(|v| v.remove("peerCancellations"))
    {
        exported.insert("cancelledCreates".into(), receipts);
    }
    let mut interrupted = BTreeSet::new();
    let asks = value
        .get_mut("asks")
        .and_then(Value::as_object_mut)
        .context("ask store is missing its asks object")?;
    for (id, ask) in asks {
        let peer = ask.get("remote").is_some_and(|v| !v.is_null())
            || ask.get("peerInbound").is_some_and(|v| !v.is_null())
            || ask.pointer("/requester/kind").and_then(Value::as_str) == Some("peer");
        let inherited = ask.get("hops").and_then(Value::as_u64).unwrap_or(0) > 0;
        if !peer && !inherited {
            continue;
        }
        exported.insert(id.clone(), ask.clone());
        let incoming = ask.pointer("/requester/kind").and_then(Value::as_str) == Some("peer");
        let direction = if incoming {
            "inbound"
        } else if peer {
            "outbound"
        } else {
            "local ancestry"
        };
        let before = ask
            .get("execution")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let open = ask.get("closed").is_none_or(Value::is_null);
        let obj = ask.as_object_mut().context("invalid ask record")?;
        for field in PEER_FIELDS {
            obj.remove(*field);
        }
        if incoming {
            obj.insert("requester".into(), json!({"kind":"owner"}));
        }
        // Local descendants keep their ordinary task; only the ancestry is stripped.
        if peer {
            obj.remove("hold");
            obj.remove("holdSince");
            obj.remove("gate");
            obj.remove("await");
            obj.remove("resume");
            obj.remove("submitRetry");
            if open {
                interrupted.insert(id.clone());
                obj.insert("execution".into(), json!("interrupted"));
                obj.insert("closed".into(), json!("stopped"));
                obj.insert("closedAt".into(), json!(at));
                obj.remove("review");
            }
            if let Some(attempts) = obj.get_mut("attempts").and_then(Value::as_array_mut) {
                for attempt in attempts {
                    let active = matches!(
                        attempt.get("state").and_then(Value::as_str),
                        Some("submitting" | "admitted" | "running" | "yielded")
                    );
                    if active || attempt.get("interruptReason").is_some() {
                        attempt["interruptReason"] = json!("stopped");
                        attempt["lastError"] =
                            json!("peerStripped: cross-host delegation exported for rollback");
                    }
                    if active {
                        attempt["state"] = json!("interrupted");
                        attempt["endedAt"] = json!(at);
                    }
                }
            }
            obj.insert("updatedAt".into(), json!(at));
            let revision = obj.get("revision").and_then(Value::as_u64).unwrap_or(0);
            obj.insert("revision".into(), json!(revision + 1));
        }
        changes.push(Change {
            ask_id: id.clone(),
            direction: direction.into(),
            before,
            after: obj
                .get("execution")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .into(),
            reason: "peerStripped",
        });
    }
    let mut state: AskState = serde_json::from_value(value).context("validate stripped store")?;
    state.leases.retain(|_, lease| {
        !matches!(&lease.holder,
        LeaseHolder::Ask { ask_id, .. } if interrupted.contains(ask_id))
    });
    state.commands.retain(|queued| match &queued.command {
        RuntimeCommand::CancelJobs { ask_id, .. } | RuntimeCommand::StopAttempt { ask_id, .. } => {
            !interrupted.contains(ask_id)
        }
        _ => true,
    });
    for question in state.questions.values_mut() {
        if interrupted.contains(&question.ask_id)
            && question.state == crate::asks::model::QuestionState::Open
        {
            question.state = crate::asks::model::QuestionState::Stale;
        }
    }
    for id in &interrupted {
        if let Some(ask) = state.asks.get(id).cloned() {
            debug_assert_eq!(ask.execution, ExecutionState::Interrupted);
            crate::asks::delivery::record_state_delivery(&mut state, &ask, "peerStripped", now);
        }
    }
    state.revision += 1;
    let mut stripped = serde_json::to_value(state)?;
    stripped
        .as_object_mut()
        .unwrap()
        .remove("peerCancellations");
    // Serialization of future internal defaults must not reintroduce peer fields.
    for ask in stripped["asks"]
        .as_object_mut()
        .context("invalid asks")?
        .values_mut()
    {
        for field in PEER_FIELDS {
            ask.as_object_mut().context("invalid ask")?.remove(*field);
        }
    }
    Ok((stripped, changes, Value::Object(exported)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        let make = |id: &str| {
            json!({
                "askId":id,"actionId":id,"requestSha256":"fixture",
                "requester":{"kind":"agent","cutex_session_id":"cutex.requester"},
                "targetCutexSessionId":"cutex.target","title":"rollback",
                "body":{"text":"hello","textBytes":5,"textSha256":"fixture"},
                "options":{},"seq":1,"execution":"queued","attempts":[],
                "createdAt":"2026-10-11T00:00:00Z","updatedAt":"2026-10-11T00:00:00Z","revision":1
            })
        };
        let mut outgoing = make("ask-outgoing");
        outgoing["remote"] = json!({"fixture":"exported without requiring current wire shape"});
        outgoing["hold"] = json!({"reason":"peerVersionMismatch"});
        let mut incoming = make("ask-incoming");
        incoming["requester"] = json!({"kind":"peer","peerId":"ci-peer","name":"remote","label":"beta","cutex_session_id":"cutex.remote"});
        incoming["execution"] = json!("running");
        incoming["peerInbound"] = json!({"fixture":"outbox"});
        json!({"revision":4,"asks":{"ask-outgoing":outgoing,"ask-incoming":incoming,"ask-local":make("ask-local")}})
    }

    #[test]
    fn strips_unknown_peer_variants_and_preserves_local_asks() {
        let input = fixture();
        let local: crate::asks::model::AskRecord =
            serde_json::from_value(input["asks"]["ask-local"].clone()).unwrap();
        let (output, changes, export) = convert(input, chrono::Utc::now()).unwrap();
        let state: AskState = serde_json::from_value(output.clone()).unwrap();
        assert_eq!(state.asks["ask-local"], local);
        assert_eq!(changes.len(), 2);
        assert_eq!(
            export["ask-outgoing"]["hold"]["reason"],
            "peerVersionMismatch"
        );
        for id in ["ask-outgoing", "ask-incoming"] {
            assert_eq!(output["asks"][id]["execution"], "interrupted");
            assert_eq!(output["asks"][id]["closed"], "stopped");
            assert!(output["asks"][id].get("remote").is_none());
            assert!(output["asks"][id].get("hold").is_none());
        }
        assert_eq!(
            output["asks"]["ask-incoming"]["requester"],
            json!({"kind":"owner"})
        );
        assert_eq!(state.deliveries.len(), 1);
        assert!(state
            .deliveries
            .values()
            .next()
            .unwrap()
            .ref_id
            .starts_with("peerStripped@"));
    }

    #[test]
    fn live_native_target_refuses_offline_conversion() {
        let mut sessions = crate::session::model::CutexSessionStore::default();
        let mut record = crate::session::model::CutexSessionRecord::new(
            "cutex.target".into(),
            None,
            "fixture-host".into(),
            std::env::temp_dir().to_string_lossy().into(),
            None,
        )
        .unwrap();
        record.runtime_pid = Some(std::process::id());
        sessions.sessions.insert("cutex.target".into(), record);
        let error = ensure_peer_targets_stopped(&fixture(), &sessions).unwrap_err();
        assert!(error.to_string().contains("live or unverifiable PID"));
        sessions
            .sessions
            .get_mut("cutex.target")
            .unwrap()
            .runtime_pid = None;
        ensure_peer_targets_stopped(&fixture(), &sessions).unwrap();
    }

    #[test]
    fn daemon_guard_and_listener_refuse_strip_before_backup_or_write() {
        let root = std::env::temp_dir().join(format!("cutex-strip-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("asks")).unwrap();
        let original = serde_json::to_vec(&fixture()).unwrap();
        std::fs::write(root.join("asks/asks-v1.json"), &original).unwrap();
        let out = root.join("export");
        let guard = migration_lock(&root).unwrap();
        FileExt::try_lock_shared(&guard).unwrap();
        assert!(export_at(&root, &[], Some(&out)).is_err());
        drop(guard);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert!(export_at(&root, &[listener.local_addr().unwrap().port()], Some(&out)).is_err());
        assert!(!out.exists());
        assert_eq!(
            std::fs::read(root.join("asks/asks-v1.json")).unwrap(),
            original
        );
        drop(listener);
        let report = export_at(&root, &[], Some(&out)).unwrap();
        assert_eq!(std::fs::read(report.backup).unwrap(), original);
        assert!(report.export_dir.join("README.txt").is_file());
        let stripped: AskState =
            serde_json::from_slice(&std::fs::read(report.store).unwrap()).unwrap();
        assert_eq!(
            stripped.asks["ask-outgoing"].execution,
            ExecutionState::Interrupted
        );
        assert!(crate::management::v2::hot_store::verified_rows(
            &root
                .join("management-v2")
                .join(crate::management::v2::hot_store::DATABASE_FILE),
            "asks"
        )
        .unwrap()
        .is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rollback_export_then_strip_reimports_and_hands_off_again() {
        let root =
            std::env::temp_dir().join(format!("cutex-strip-handoff-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("asks")).unwrap();
        let (current, _, _) = convert(fixture(), chrono::Utc::now()).unwrap();
        std::fs::write(
            root.join("asks/asks-v1.json"),
            serde_json::to_vec(&current).unwrap(),
        )
        .unwrap();
        let store = crate::asks::store::AskStore::at_runtime(&root);
        store.snapshot().unwrap();
        assert!(store.export_legacy_json().unwrap());

        let report = export_at(&root, &[], Some(&root.join("export"))).unwrap();
        let mut legacy: AskState =
            serde_json::from_slice(&std::fs::read(&report.store).unwrap()).unwrap();
        legacy.next_seq = 77;
        write_private_pretty_json_atomic(&report.store, &legacy, "simulated old ask store")
            .unwrap();
        let reopened = crate::asks::store::AskStore::at_runtime(&root)
            .snapshot()
            .unwrap();
        assert_eq!(
            reopened.next_seq, 77,
            "roll-forward imports changes made by the old binary"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
