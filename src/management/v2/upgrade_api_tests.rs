use super::*;
use crate::upgrade::manifest::MigrationClass;
use crate::upgrade::state::{Phase, Run, RunKind};

fn assert_valid(definition: &str, value: &Value) {
    let validator = super::super::schema_export::validator("upgrade", definition).unwrap();
    let errors: Vec<String> = validator.iter_errors(value).map(|e| format!("{} at {}", e, e.instance_path)).collect();
    assert!(errors.is_empty(), "{definition}: {errors:?}\n{value:#}");
}

#[test]
fn routes() {
    assert_eq!(parse_route("GET", "/v2/upgrade"), Ok(Route::Status));
    assert_eq!(parse_route("POST", "/v2/upgrade/check"), Ok(Route::Check));
    assert_eq!(parse_route("POST", "/v2/upgrade/stage"), Ok(Route::Stage));
    assert_eq!(parse_route("POST", "/v2/upgrade/apply"), Ok(Route::Apply));
    assert_eq!(parse_route("POST", "/v2/upgrade/cancel"), Ok(Route::Cancel));
    assert_eq!(parse_route("POST", "/v2/upgrade/rollback"), Ok(Route::Rollback));
    assert_eq!(parse_route("GET", "/v2/upgrade/runs"), Ok(Route::Runs));
    assert_eq!(parse_route("GET", "/v2/upgrade/runs/upg-0123456789ab"), Ok(Route::Run("upg-0123456789ab".into())));
    assert_eq!(parse_route("PATCH", "/v2/upgrade/settings"), Ok(Route::Settings));
    assert_eq!(parse_route("GET", "/v2/upgrade/settings"), Ok(Route::Settings));
    assert_eq!(parse_route("POST", "/v2/upgrade"), Err(true));
    assert_eq!(parse_route("GET", "/v2/upgrade/apply"), Err(true));
    assert_eq!(parse_route("GET", "/v2/upgrade/runs/../x"), Err(false));
    assert_eq!(parse_route("GET", "/v2/upgrade/other"), Err(false));
    assert!(is_path("/v2/upgrade") && is_path("/v2/upgrade/runs") && !is_path("/v2/upgrades"));
}

#[test]
fn bodies() {
    let request = parse_apply(br#"{"release":"r240","mode":"full","maxWaitSecs":60,"confirmOneWay":"r240","packageRun":"during"}"#).unwrap();
    assert_eq!(request.release.as_deref(), Some("r240"));
    assert_eq!(request.mode, Some(Mode::Full));
    assert_eq!(request.package_run, Some(PackageRun::During));
    assert_eq!(request.max_wait_secs, Some(60));
    assert!(parse_apply(b"").unwrap().release.is_none());
    assert!(parse_apply(br#"{"mode":"half"}"#).is_err());
    assert!(parse_apply(br#"{"force":true}"#).unwrap_err().contains("force"));
    assert!(parse_apply(br#"{"maxWaitSecs":999999}"#).is_err());
    let rollback = parse_rollback(br#"{"dryRun":true,"to":"r239"}"#).unwrap();
    assert!(rollback.dry_run && rollback.to.as_deref() == Some("r239"));
    assert!(parse_rollback(br#"{"dryRun":"yes"}"#).is_err());

    let mut settings = Settings::default();
    patch_settings(&mut settings, br#"{"channel":"stable","autoApply":true,"packageRun":"none","packageRunBatch":3,"checkIntervalHours":0}"#).unwrap();
    assert_eq!(settings.channel, "release");
    assert!(settings.auto_apply);
    assert_eq!((settings.package_run, settings.package_run_batch, settings.check_interval_hours), (PackageRun::None, 3, 0));
    assert_eq!(patch_settings(&mut settings, br#"{"source":"https://example.invalid"}"#).unwrap_err().0, "source_not_editable");
    assert_eq!(patch_settings(&mut settings, br#"{"channel":"nightly"}"#).unwrap_err().0, "invalid_request");
    assert_eq!(patch_settings(&mut settings, br#"{"packageRunBatch":0}"#).unwrap_err().0, "invalid_request");
}

#[test]
fn due_checks() {
    assert!(due(None, 6));
    assert!(!due(None, 0));
    let recent = chrono::Utc::now().to_rfc3339();
    assert!(!due(Some(&recent), 6));
    assert!(due(Some("2020-01-01T00:00:00Z"), 6));
}

#[test]
fn unmanaged_status_and_settings_match_the_schema() {
    let layout = Layout::at(std::env::temp_dir().join(format!("cutex-upgrade-api-{}", uuid::Uuid::new_v4().simple())));
    let status = control::status_view(&layout);
    assert_eq!(status["managed"], false);
    assert_valid("upgradeStatus", &status);
    assert_valid("upgradeSettingsResponse", &json!({ "settings": Settings::default().view(&layout) }));
    let error = start_stage(&layout, None).unwrap_err();
    assert_eq!(error.downcast_ref::<UpgradeError>().unwrap().details["reason"], "unmanaged_install");
    let _ = std::fs::remove_dir_all(&layout.base);
}

#[test]
fn an_unmanaged_daemon_reports_unmanaged_and_refuses_mutations() {
    // The test binary sits in no layout position: no managed install, and
    // never a stand-in base (a shared temp dir anyone could seed).
    if std::env::var_os(crate::upgrade::layout::INSTALL_BASE_ENV).is_none() {
        assert!(layout().is_none());
    }
    let (status, view) = unmanaged("GET", &Route::Status).unwrap();
    assert_eq!((status, &view["managed"], &view["base"]), (200, &json!(false), &Value::Null));
    assert_eq!(view["installedReleases"], json!([]));
    assert_valid("upgradeStatus", &view);
    let (_, settings) = unmanaged("GET", &Route::Settings).unwrap();
    assert_valid("upgradeSettingsResponse", &settings);
    assert_eq!(unmanaged("GET", &Route::Runs).unwrap().1, json!({ "runs": [] }));
    let missing = unmanaged("GET", &Route::Run("upg-0123456789ab".into())).unwrap_err();
    assert_eq!(missing.downcast_ref::<UpgradeError>().unwrap().code, "run_not_found");
    for (method, route) in [
        ("PATCH", Route::Settings),
        ("POST", Route::Check),
        ("POST", Route::Stage),
        ("POST", Route::Apply),
        ("POST", Route::Cancel),
        ("POST", Route::Rollback),
    ] {
        let error = unmanaged(method, &route).unwrap_err();
        let error = error.downcast_ref::<UpgradeError>().unwrap();
        assert_eq!((error.status, error.code, &error.details["reason"]), (409, "upgrade_blocked", &json!("unmanaged_install")), "{route:?}");
    }
}

#[test]
fn runs_and_events_match_the_schema() {
    let mut run = Run::new(RunKind::Apply, "r239", "r240", Mode::Full, MigrationClass::RollbackSafe, 3600, PackageRun::After);
    assert_valid("upgradeRunResponse", &json!({ "run": run }));
    run.waiting_for.push(crate::upgrade::state::Busy { reason: "activeTurn".into(), cutex_session_id: Some("cutex.x".into()), name: None, detail: None });
    run.enter(Phase::Stopping, None);
    run.fail(Phase::NeedsOwner, "verify_failed", "capabilities missing");
    run.needs_owner = Some(crate::upgrade::state::NeedsOwner { reason: "x".into(), commands: vec!["cutex upgrade status".into()] });
    run.extra.insert("selectionBackup".into(), json!("/b"));
    assert_valid("upgradeRunList", &json!({ "runs": [run.clone()] }));
    run.set_runtime(Some("0.162.1-r53".into()), Some("0.160.0-r51".into()));
    assert_valid("upgradeRunResponse", &json!({ "run": run }));
    let mut invalid = serde_json::to_value(&run).unwrap();
    invalid["runtimeChange"] = json!("other");
    assert!(super::super::schema_export::validator("upgrade", "upgradeRun").unwrap().iter_errors(&invalid).next().is_some());
    let mut state = State::default();
    state.run = Some(run);
    state.extra.insert(STAGING_KEY.into(), json!({ "release": "r241", "state": "failed", "error": { "code": "signature_invalid", "message": "m" } }));
    let event = event_params(&state, "state");
    assert_eq!(event["run"]["phase"], "needs-owner");
    assert_valid("upgradeEvent", &event);
    assert_valid("upgradeEvent", &event_params(&State::default(), "startup"));
}

#[cfg(unix)]
#[test]
fn managed_status_with_staged_release_matches_the_schema() {
    use crate::upgrade::minisign::{test_key::TestKey, PublicKey};
    use crate::upgrade::stage::fixtures::{installed_layout, publish, temp};
    let key = TestKey::new(9);
    let keys = vec![PublicKey::parse(&key.public_text()).unwrap()];
    let layout = installed_layout("r239", 40);
    let root = temp("api-src");
    publish(&root, &key, "r240", 41, Some("0.162.1-r53"), |_| {});
    let source = crate::upgrade::source::Source::parse(&root.display().to_string()).unwrap();
    stage::check(&layout, &source, &keys, "release", &stage::LocalRuntimes::default()).unwrap();
    stage::stage(&layout, &source, &keys, "release", "r240", &stage::LocalRuntimes::default()).unwrap();
    let status = control::status_view(&layout);
    assert_eq!(status["managed"], true);
    assert_eq!(status["staged"]["release"], "r240");
    assert_eq!(status["available"]["mode"], "full");
    assert!(status["staging"].is_null());
    assert_valid("upgradeStatus", &status);
    // A failed background stage is visible in the status (not only in events).
    set_staging(&layout, json!({
        "release": "r241", "state": "failed", "finishedAt": "2026-10-15T00:00:00Z",
        "error": { "code": "signature_invalid", "message": "cutex-r241/manifest.json: signature verification failed" },
    }));
    let status = control::status_view(&layout);
    assert_eq!(status["staging"]["error"]["code"], "signature_invalid");
    assert_valid("upgradeStatus", &status);
    let (_, preview) = {
        crate::upgrade::swap::activate(&layout, "r240").unwrap();
        control::prepare_rollback(&layout, &RollbackRequest { dry_run: true, ..Default::default() }).unwrap()
    };
    assert_valid("rollbackResponse", &json!({ "preview": preview }));
    std::fs::remove_dir_all(&layout.base).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}
