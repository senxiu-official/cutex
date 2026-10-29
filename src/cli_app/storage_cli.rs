//! Explicit offline exports for rolling durable storage back to an older cutex.

use std::path::Path;

use anyhow::Context;
use cutex::cli::args::StorageCommand;

pub(super) fn run(command: StorageCommand) -> anyhow::Result<()> {
    match command {
        StorageCommand::AsksArchive {before,daemon_stopped} => {
            anyhow::ensure!(daemon_stopped,"stop every cutex writer, then repeat with --daemon-stopped");
            let now=chrono::Utc::now();
            let requested=before.as_deref().map(|value|cutex::asks::archive::activation_cutoff(now,Some(value))).transpose()?;
            let store=cutex::asks::store::AskStore::open_default()?;
            if let Some(cutoff)=requested {store.configure_archive_cutoff(cutoff,true)?;}
            let report=store.archive_before(None,now)?;
            println!("{}",serde_json::to_string_pretty(&report)?);
            Ok(())
        },
        StorageCommand::AsksArchiveRestore {target,daemon_stopped} => {
            anyhow::ensure!(daemon_stopped,"stop every cutex writer, then repeat with --daemon-stopped");
            anyhow::ensure!(matches!(target.as_str(),"r259"|"r260"|"r261"),"asks archive restore supports only the preceding kit target r259/r260/r261");
            if target=="r259" {
                cutex::upgrade::older_stores::preflight_asks_archive_restore(&cutex::config::paths::config_dir()?,&target)?;
            }
            let report=cutex::asks::store::AskStore::open_default()?.restore_archived_for_rollback(true)?;
            println!("{}",serde_json::to_string_pretty(&serde_json::json!({"target":target,"restored":report,"daemonStopped":true}))?);
            Ok(())
        },
        StorageCommand::RollbackExport {
            target,
            daemon_stopped,
        } => rollback_export(&target, daemon_stopped),
    }
}

fn rollback_export(target: &str, daemon_stopped: bool) -> anyhow::Result<()> {
    anyhow::ensure!(daemon_stopped, "stop cutex services, then repeat with --daemon-stopped");
    let home = cutex::config::paths::config_dir()?;
    let result = rollback_export_at(&home, target, daemon_stopped)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn rollback_export_at(home: &Path, target: &str, daemon_stopped: bool) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(
        daemon_stopped,
        "stop cutex services, then repeat with --daemon-stopped"
    );
    // Refuse known blockers before creating the rollback tree or backup.
    // The stopped-writer prepare/export path repeats this read-only pass.
    cutex::upgrade::older_stores::preflight(home, target)?;
    if matches!(target, "r262" | "r263" | "r264") {
        // These predecessors read the hot/cold contracts. Do not
        // export legacy JSON, restore cold asks, or create a downgrade tree.
        // This format-only result does not waive kit archive/pending/retry
        // guards, stopped-writer fencing or consistent backups.
        return Ok(serde_json::json!({"target":target,"daemonStopped":true,
            "exports":[],"displaced":null,"backup":null,"hotStoresPreserved":true}));
    }
    let rollback_dir = home
        .join("rollback-export")
        .join(uuid::Uuid::new_v4().to_string());
    let displaced = rollback_dir.join("displaced");
    let backup = (target != "r256").then(|| rollback_dir.join("backup"));
    if let Some(backup) = &backup {
        cutex::upgrade::backup::backup(home, backup)
            .with_context(|| format!("create pre-rollback backup at {}", backup.display()))?;
    }
    let notes = if target == "r256" {
        cutex::upgrade::older_stores::export_hot_stores(home, target, true)?
    } else {
        cutex::upgrade::older_stores::prepare(home, &displaced, target).with_context(|| {
            format!(
                "rollback export failed; services must remain stopped; restore or reconcile from {}",
                backup.as_ref().expect("older target backup").display()
            )
        })?
    };

    Ok(serde_json::json!({
        "target": target,
        "daemonStopped": true,
        "exports": notes,
        "displaced": displaced,
        "backup": backup,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    include!("rollback_r263_tests.rs");

    #[test]
    fn r264_cli_preserves_empty_and_real_archive_contract_without_conversion() {
        let f = R263Fixture::new();
        check_r264(&f, true);
        let mut receipt = old_archive_receipt();
        receipt["request"]["review"]["operation"] = json!("permanent_retire");
        receipt["stage"] = json!("permanent_retirement_committed");
        receipt["archiveKind"] = json!("permanent");
        receipt["restorable"] = json!(false);
        let marker = json!({"minimum_reader_release":"r264","binary_only_rollback_blocked_to":["r263"],
            "reason":"archive_receipt_fields_and_permanent_enum_variants"});
        f.write(
            "cutex-sessions.json",
            &json!({"sessions":{},
            "archive_storage_contract_v1":marker,"agent_archive_receipts":{"fixture":receipt}}),
        );
        check_r264(&f, true);
        f.check(false); // r263 still cannot read this archive contract.
        let before = f.tree();
        assert!(super::rollback_export_at(&f.0, "r264", false).is_err());
        assert_eq!(f.tree(), before);
        receipt["stage"] = json!("future");
        f.write(
            "cutex-sessions.json",
            &json!({"sessions":{},"agent_archive_receipts":{"fixture":receipt}}),
        );
        check_r264(&f, false);
    }

    fn check_r264(f: &R263Fixture, allowed: bool) {
        let before = f.tree();
        let preflight = cutex::upgrade::older_stores::preflight(&f.0, "r264");
        assert_eq!(preflight.is_ok(), allowed, "{preflight:?}");
        let result = super::rollback_export_at(&f.0, "r264", true);
        assert_eq!(result.is_ok(), allowed, "{result:?}");
        if allowed {
            assert_eq!(
                result.unwrap(),
                json!({"target":"r264","daemonStopped":true,
            "exports":[],"displaced":null,"backup":null,"hotStoresPreserved":true})
            );
        }
        assert_eq!(
            f.tree(),
            before,
            "r264 CLI/preflight changed source tree/bytes/identity"
        );
    }

    #[test]
    fn predecessor_cli_checks_required_session_revision_including_wal() {
        let f = R263Fixture::new();
        let db = rusqlite::Connection::open(f.0.join("cutex-sessions.sqlite3")).unwrap();
        db.execute_batch("PRAGMA user_version=1;
            CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            INSERT INTO meta VALUES('store_revision','1');
            CREATE TABLE sessions(key TEXT PRIMARY KEY,rev INTEGER NOT NULL,record_json TEXT NOT NULL);
            CREATE TABLE receipts(map TEXT NOT NULL,key TEXT NOT NULL,rev INTEGER NOT NULL,json TEXT NOT NULL,PRIMARY KEY(map,key));
            PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;").unwrap();
        db.execute("INSERT INTO receipts VALUES('agent_archive','fixture',1,?1)",
            [old_archive_receipt().to_string()]).unwrap();
        f.check(true); check_r264(&f, true);
        for revision in ["bad", "-1", "18446744073709551616"] {
            db.execute("UPDATE meta SET value=?1 WHERE key='store_revision'", [revision]).unwrap();
            f.check(false); check_r264(&f, false);
        }
        db.execute_batch("DELETE FROM meta").unwrap();
        f.check(false); check_r264(&f, false);
        db.execute_batch("INSERT INTO meta VALUES('store_revision','1'); UPDATE receipts SET rev=-1").unwrap();
        f.check(false); check_r264(&f, false);
        db.execute_batch("UPDATE receipts SET rev=1").unwrap();
        f.check(true); check_r264(&f, true);
    }

    #[test]
    fn predecessor_cli_retains_historical_digests_and_refuses_future_markers() {
        let f = R263Fixture::new();
        let mut receipt = old_archive_receipt();
        for key in ["durable_sha256", "authority_sha256"] {
            receipt["request"]["review"][key] = json!("b".repeat(64));
            f.write("cutex-sessions.json", &json!({"sessions":{},"agent_archive_receipts":{"fixture":receipt}}));
            f.check(true); check_r264(&f, true);
        }
        f.write("cutex-sessions.json", &json!({"sessions":{},"archive_storage_contract_v2":{
            "minimum_reader_release":"r265"},"agent_archive_receipts":{"fixture":receipt}}));
        f.check(false); check_r264(&f, false);
    }

    #[test]
    fn r264_cli_keeps_original_retry_settlement_and_pending_stop_gates() {
        let f = R263Fixture::new();
        f.write(RETRY_GUARD, &inert_retry());
        check_r264(&f, true);
        // Existing retry obligations are unrelated to the removed bus feature.
        for field in ["plan", "settlement"] {
            let mut retry = inert_retry();
            retry["sessions"]["cutex.fixture"][field] = json!({"phase":"uncertain"});
            f.write(RETRY_GUARD, &retry);
            check_r264(&f, false);
        }
        f.write(RETRY_GUARD, &inert_retry());
        let path = f.0.join(PENDING_GUARD);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(PENDING_SCHEMA).unwrap();
        check_r264(&f, true);
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
            INSERT INTO pending_stop VALUES(1,'s','9','r','h','Offline',0,0)",
        )
        .unwrap();
        check_r264(&f, false);
        db.execute_batch("DELETE FROM pending_stop").unwrap();
        check_r264(&f, true);
        drop(db);
    }

    #[cfg(windows)]
    #[test]
    fn r264_cli_denied_and_oversize_inputs_are_not_absence() {
        use std::os::windows::fs::OpenOptionsExt;
        let f = R263Fixture::new();
        f.write(RETRY_GUARD, &inert_retry());
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(f.0.join(RETRY_GUARD))
            .unwrap();
        assert!(super::rollback_export_at(&f.0, "r264", true).is_err());
        drop(held);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(f.0.join(RETRY_GUARD))
            .unwrap();
        file.set_len(64 * 1024 * 1024 + 1).unwrap();
        drop(file);
        check_r264(&f, false);
        f.write(RETRY_GUARD, &inert_retry());
        let original = f.0.join("runtime/model-overload-retry");
        let private_target = f.0.join("private-target");
        std::fs::rename(&original, &private_target).unwrap();
        let created = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(original.to_string_lossy().replace('/', "\\"))
            .arg(private_target.to_string_lossy().replace('/', "\\"))
            .output()
            .unwrap();
        assert!(created.status.success(), "private junction fixture failed");
        let saved = std::fs::read(private_target.join("state-v1.json")).unwrap();
        let refused = super::rollback_export_at(&f.0, "r264", true).is_err();
        // Remove only the private fixture junction before recursive RAII cleanup.
        std::fs::remove_dir(&original).unwrap();
        assert!(refused);
        assert_eq!(
            std::fs::read(private_target.join("state-v1.json")).unwrap(),
            saved
        );
        assert!(!f.0.join("rollback-export").exists());
    }

    #[test]
    fn r264_cli_v7_success_unknown_version_and_orphan_sidecar_refusal() {
        let f = R263Fixture::new();
        let bus = "runtime/management-v2/agent-bus-message-state.json";
        f.write(bus, &json!({"version":7,"messages":{}}));
        check_r264(&f, true);
        for version in [json!(8), json!(99), json!("bad")] {
            f.write(bus, &json!({"version":version,"messages":{}}));
            check_r264(&f, false);
        }
        f.write(bus, &json!({"version":7,"messages":{}}));
        f.raw(
            "runtime/management-v2/management-state.sqlite3-wal",
            b"orphan",
        );
        check_r264(&f, false);
    }

    #[test]
    fn archive_commands_refuse_before_resolving_a_home_without_offline_confirmation() {
        use cutex::cli::args::StorageCommand;
        for command in [StorageCommand::AsksArchive {before:None,daemon_stopped:false},
            StorageCommand::AsksArchiveRestore {target:"r261".into(),daemon_stopped:false}] {
            assert!(super::run(command).unwrap_err().to_string().contains("--daemon-stopped"));
        }
    }

    #[test]
    fn rollback_export_requires_an_explicit_offline_confirmation() {
        let error = super::rollback_export("r256", false).unwrap_err();
        assert!(error.to_string().contains("--daemon-stopped"));
    }

    #[test]
    fn immediate_r262_export_validates_without_creating_a_downgrade_tree() {
        let home = std::env::temp_dir().join(format!("cutex-r262-noop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let result = super::rollback_export_at(&home, "r262", true).unwrap();
        assert_eq!(result["hotStoresPreserved"], true);
        assert_eq!(result["exports"], json!([]));
        assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);
        let r263 = super::rollback_export_at(&home, "r263", true).unwrap();
        assert_eq!(r263, json!({"target":"r263","daemonStopped":true,
            "exports":[],"displaced":null,"backup":null,"hotStoresPreserved":true}));
        assert_eq!(super::rollback_export_at(&home, "r264", true).unwrap(),
            json!({"target":"r264","daemonStopped":true,"exports":[],
                "displaced":null,"backup":null,"hotStoresPreserved":true}));
        assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn r263_cli_corruption_refuses_without_backup_or_migration() {
        let home = std::env::temp_dir().join(format!("cutex-cli-r263-{}", uuid::Uuid::new_v4()));
        let path = home.join("runtime/asks/asks-v1.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"corrupt").unwrap();
        assert!(super::rollback_export_at(&home, "r263", true).is_err());
        assert!(!home.join("rollback-export").exists());
        assert!(!home.join("runtime/management-v2").exists());
        assert_eq!(std::fs::read(path).unwrap(), b"corrupt");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn blocking_preflight_writes_no_backup_or_rollback_tree() {
        let home = std::env::temp_dir().join(format!("cutex-storage-cli-preflight-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let state = json!({
            "version": 1,
            "revision": 1,
            "alarmRoles": {
                "alarm-1": {"owner": "cutex.owner", "projectId": "project", "rawDigest": "digest", "target": "cutex.director"}
            }
        });
        cutex::config::atomic::write_private_pretty_json_atomic(
            &home.join("runtime/director-address/ownership-v1.json"), &state, "Director downgrade preflight fixture",
        ).unwrap();

        let error = super::rollback_export_at(&home, "r252", true).unwrap_err();
        assert!(error.to_string().contains("director_targeted_alarm_role"), "{error:#}");
        assert!(!home.join("rollback-export").exists(), "preflight refusal must not create backup output");
        std::fs::remove_dir_all(home).unwrap();
    }
}
