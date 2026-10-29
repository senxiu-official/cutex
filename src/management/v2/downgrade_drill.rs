//! Synthetic fixture producer for an explicitly selected older binary. No live
//! instance, runtime, credentials, service or model is used by this test.
use serde_json::json;
use std::{fs, path::Path};

fn write(path: &Path, value: &serde_json::Value) {
    crate::config::atomic::write_private_pretty_json_atomic(
        path,
        value,
        "synthetic downgrade fixture",
    )
    .unwrap();
}

#[test]
#[ignore = "requires CUTEX_R257_DOWNGRADE_FIXTURE under private TEMP; produces a retained synthetic fixture"]
fn write_synthetic_upgraded_home_for_older_binary_drill() {
    let root = std::path::PathBuf::from(
        std::env::var_os("CUTEX_R257_DOWNGRADE_FIXTURE").expect("explicit fixture root"),
    );
    assert!(root.starts_with(std::env::temp_dir()));
    assert!(root
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("cutex-r257-downgrade-fixture-"));
    assert!(
        !root.exists(),
        "never overwrite an existing fixture or home"
    );
    fs::create_dir_all(&root).unwrap();
    let home = root.join(".cutex");
    fs::create_dir_all(&home).unwrap();
    let mut config = crate::profiles::model::CodezConfig::default();
    config.profile_quota_refresh_minutes = 0;
    config.claude_usage_refresh_minutes = 0;
    write(
        &home.join("config.json"),
        &serde_json::to_value(&config).unwrap(),
    );
    let profile_id = "11111111-1111-4111-8111-111111111111";
    let mut accounts = crate::profiles::model::AccountsStore::default();
    accounts.accounts.push(serde_json::from_value(json!({
        "id":profile_id,"name":"Upgraded fixture","email":null,"plan_type":null,"last_used_at":null,
        "options":{"account_link":"manual:22222222-2222-4222-8222-222222222222",
            "intro_greeting":"Synthetic appearance", "appearance_updated_at":{"introGreeting":"2026-10-24T00:00:00Z"}}
    })).unwrap());
    write(
        &home.join("accounts.json"),
        &serde_json::to_value(&accounts).unwrap(),
    );
    let session_id = "cutex.33333333-3333-4333-8333-333333333333";
    let mut record = crate::session::model::CutexSessionRecord::new_at(
        session_id.into(),
        None,
        crate::platform::host::current_host_name(),
        root.to_string_lossy().into(),
        None,
        "2026-10-24T00:00:00Z".into(),
    )
    .unwrap();
    record.agent_enabled = true;
    record.badge = Some(crate::session::model::CutexSessionBadge {
        text: "AB".into(),
        tint: None,
    });
    record.provider_family = Some("anthropic".into());
    let mut sessions = crate::session::model::CutexSessionStore::default();
    sessions.sessions.insert(session_id.into(), record);
    crate::session::store::save_cutex_session_store_to_path(
        &home.join("cutex-sessions.json"),
        &sessions,
    )
    .unwrap();

    let director = crate::director_address::Store::at(home.join("runtime/director-address"));
    director
        .update(|state| {
            state.reviews.insert(
                "visible-history".into(),
                crate::director_address::HandoverReview {
                    id: "visible-history".into(),
                    project_id: "synthetic-project".into(),
                    ask_id: "synthetic-ask".into(),
                    from: "original".into(),
                    to: "successor".into(),
                    change_id: "synthetic-change".into(),
                    revision: 1,
                    state: "accepted".into(),
                    reason: "Synthetic visible history".into(),
                    provenance_known: true,
                    actor: Some("owner".into()),
                    owner_only: false,
                    resolved_to: None,
                },
            );
            Ok(())
        })
        .unwrap();
    let preview = json!({"previewId":"44444444-4444-4444-8444-444444444444","action":"old-logs",
        "expiresAt":"2099-01-01T00:00:00Z","items":[],"freeableBytes":0});
    super::super::storage_cleanup_state::save_preview(&home, &preview, 4_070_908_800).unwrap();
    let mut run = json!({"runId":"55555555-5555-4555-8555-555555555555", "requestId":"synthetic-request",
        "previewId":preview["previewId"],"createdAt":"2026-10-24T00:00:00Z","action":"old-logs",
        "state":"running","items":[],"actor":{"kind":"synthetic-test"}});
    super::super::storage_cleanup_state::begin(&home, &run).unwrap();
    run["state"] = json!("completed");
    super::super::storage_cleanup_state::save_run(&home, &run, true).unwrap();
    // Close/checkpoint SQLite before copying this fixture to another platform.
    super::super::storage_cleanup_state::database(&home)
        .unwrap()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let retained = [
        (
            "runtime/storage/trash/synthetic/kept.txt",
            b"synthetic retained trash".as_slice(),
        ),
        (
            "runtime/profile-link/secret-v1",
            b"synthetic-test-key-material-only!".as_slice(),
        ),
        (
            "runtime/profile-link/disabled-v1",
            b"cutex-profile-link-disabled/v1\n".as_slice(),
        ),
        (
            "generated-image-history/synthetic.png",
            b"synthetic inert image snapshot".as_slice(),
        ),
    ];
    for (relative, bytes) in retained {
        let path = home.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::config::atomic::write_private_bytes_atomic(&path, bytes).unwrap();
    }
    fs::create_dir_all(home.join("runtime/management-v2")).unwrap();
    fs::write(
        home.join("runtime/management-v2/integration-event-dedupe-v1.json"),
        b"{\"committed\":[\"synthetic-permanent-identity\"],\"pending\":[]}",
    )
    .unwrap();
    assert_eq!(
        super::super::integration_events::prepare_synthetic_downgrade_ledger(
            &home.join("runtime/management-v2")
        )
        .unwrap(),
        1
    );
    let blockers = crate::profiles::account_link::legacy_downgrade_blockers(&home).unwrap();
    assert_eq!(blockers, ["profile_link_private_state", "profile_manual_link_metadata", "profile_appearance_clock_metadata"]);
    let manifest = json!({"synthetic":true,"source":"r257 current writers","legacyDowngradeBlockers":blockers,
        "dedupeExportedFromV2":true,"profileId":profile_id,
        "sessionId":session_id,"directorReview":"visible-history","cleanupRunId":run["runId"],"cleanupPreviewId":preview["previewId"],
        "nativeSavedGate":"inert; no native runtime upgraded or launched"});
    write(&root.join("fixture.json"), &manifest);
    #[cfg(windows)]
    crate::platform::private_fs::secure_tree(&root).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

#[test]
#[ignore = "requires explicitly downloaded public 0.1.5 assets; never reads an installed binary"]
fn verify_public_015_download_with_embedded_release_keys() {
    use sha2::Digest;
    let root = std::path::PathBuf::from(
        std::env::var_os("CUTEX_R257_PUBLIC_BASELINE").expect("explicit public asset directory"),
    );
    let keys: Vec<_> = crate::upgrade::keys::EMBEDDED_PUBLIC_KEYS
        .iter()
        .map(|key| crate::upgrade::minisign::PublicKey::parse(key).unwrap())
        .collect();
    let manifest = fs::read(root.join("manifest.json")).unwrap();
    crate::upgrade::minisign::verify_text(
        &keys,
        &manifest,
        &fs::read_to_string(root.join("manifest.json.minisig")).unwrap(),
    )
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
    assert_eq!(
        value["cutex"]["commit"],
        "8bc12abcc9a9f8d5fe126afdb1d476097fbfe4c3"
    );
    assert_eq!(value["cutex"]["release"], "0.1.5");
    let platform =
        std::env::var("CUTEX_R257_PUBLIC_PLATFORM").unwrap_or_else(|_| "windows-x64".into());
    let asset = &value["cutex"]["artifacts"][&platform];
    let name = asset["name"].as_str().unwrap();
    assert!(name.starts_with("cutex-0.1.5-") && !name.contains('/') && !name.contains('\\'));
    let bytes = fs::read(root.join(name)).unwrap();
    assert_eq!(bytes.len() as u64, asset["size"].as_u64().unwrap());
    assert_eq!(
        format!("{:x}", sha2::Sha256::digest(&bytes)),
        asset["sha256"].as_str().unwrap()
    );
    crate::upgrade::minisign::verify_text(
        &keys,
        &bytes,
        &fs::read_to_string(root.join(format!("{name}.minisig"))).unwrap(),
    )
    .unwrap();
}

#[test]
#[ignore = "run after r257_downgrade_smoke.py on the explicit synthetic upgraded fixture"]
fn reopen_fixture_after_older_binary_writes() {
    let root = std::path::PathBuf::from(
        std::env::var_os("CUTEX_R257_DOWNGRADE_FIXTURE").expect("explicit fixture root"),
    );
    assert!(root.starts_with(std::env::temp_dir()));
    assert!(root
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("cutex-r257-downgrade-fixture-"));
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("fixture.json")).unwrap()).unwrap();
    assert_eq!(manifest["synthetic"], true);
    assert_eq!(manifest["dedupeExportedFromV2"], true);
    let home = root.join(".cutex");
    let accounts: crate::profiles::model::AccountsStore =
        serde_json::from_slice(&fs::read(home.join("accounts.json")).unwrap()).unwrap();
    let profile = accounts
        .accounts
        .iter()
        .find(|p| Some(p.id.as_str()) == manifest["profileId"].as_str())
        .unwrap();
    assert_eq!(profile.name, "Edited by older binary");
    assert_eq!(
        profile.options.account_link.as_deref(),
        Some("manual:22222222-2222-4222-8222-222222222222")
    );
    assert_eq!(
        profile.options.appearance_updated_at["introGreeting"],
        "2026-10-24T00:00:00Z"
    );
    let sessions = crate::session::store::load_cutex_session_store_from_path(
        &home.join("cutex-sessions.json"),
    )
    .unwrap();
    let record = &sessions.sessions[manifest["sessionId"].as_str().unwrap()];
    assert_eq!(record.note.as_deref(), Some("Edited by old binary"));
    let director = crate::director_address::Store::at(home.join("runtime/director-address"))
        .snapshot()
        .unwrap();
    assert!(director.reviews.contains_key("visible-history"));
    let cleanup =
        super::super::storage_cleanup_state::run(&home, manifest["cleanupRunId"].as_str().unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(cleanup["state"], "completed");
    let status = crate::profiles::account_link::secret_status(&home).unwrap();
    assert_eq!(status["disabled"], true);
    assert!(status["secretId"].is_null());
    super::super::integration_events::reopen_synthetic_downgrade_ledger(
        &home.join("runtime/management-v2"),
    )
    .unwrap();
}
