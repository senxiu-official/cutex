// Included in the existing tests module so its private-home subprocess helper applies.
#[test]
fn image_auto_steer_unresolved_never_expires_or_changes_binding() {
    if test_runs_in_private_home("image_auto_steer_unresolved_never_expires_or_changes_binding") { return; }
    let (root, repository) = queue_repository("img-direct-unknown");
    let mut command = image_command("cutex.native-direct-unknown", &root, "direct-image");
    command.params.strategy = UserInputStrategy::Auto;
    let steer = ImageInputSettings { busy_behavior: ImageBusyBehavior::Steer };
    let sends = std::cell::Cell::new(0);
    let error = submit_v2_with_repository_and_image_settings(command.clone(), &repository, steer,
        |_, _| Some(Some("running".into())), |_, _| panic!("tracked"), no_history,
        |_, method, _, _| { assert_eq!(method, "turn/steer"); sends.set(sends.get() + 1); Err(outcome_unknown_error()) },
        no_flush).unwrap_err();
    assert!(error.outcome_unknown);
    assert_eq!(sends.get(), 1);
    assert!(repository.list(&command.cutex_session_id).unwrap().1.is_empty(), "exercise direct submit, not queue protection");
    drop(repository);
    // Reopen the durable store. Both transport-unknown and crash-left Sending
    // survive the old 30s idle grace and even the 30-day identity retention.
    for age in [chrono::Duration::seconds(31), chrono::Duration::days(45)] {
        for sending in [false, true] {
            let repository = UserInputRepository::open(&root).unwrap();
            let old = repository.delivery(&command.cutex_session_id, "direct-image").unwrap().unwrap();
            let since = (Utc::now() - age).to_rfc3339();
            let unresolved = if sending {
                InputDelivery::Sending { thread_id:command.thread_id.clone(), since }
            } else { InputDelivery::Unknown { thread_id:command.thread_id.clone(), since } };
            assert!(repository.set_delivery(&command.cutex_session_id, "direct-image", Some(&old), Some(unresolved.clone())).unwrap());
            for thread in [command.thread_id.as_str(), "replacement-thread"] {
                for settings in [steer, ImageInputSettings::default()] {
                    let mut retry = command.clone();
                    retry.thread_id = thread.into();
                    retry.management_request_id = uuid::Uuid::new_v4().to_string();
                    let error = submit_v2_with_repository_and_image_settings(retry, &repository, settings,
                        |_, _| Some(None), |_, _| panic!("existing identity"), idle_thread,
                        |_, _, _, _| { sends.set(sends.get() + 1); panic!("unresolved image resent") }, no_flush).unwrap_err();
                    assert_eq!(error.code, "user_input_in_progress");
                    assert_eq!(repository.delivery(&command.cutex_session_id, "direct-image").unwrap(), Some(unresolved.clone()));
                    assert!(repository.list(&command.cutex_session_id).unwrap().1.is_empty());
                }
            }
        }
    }
    assert_eq!(sends.get(), 1, "all retries made zero native sends");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn image_auto_steer_unknown_visible_in_history_deduplicates_without_send() {
    if test_runs_in_private_home("image_auto_steer_unknown_visible_in_history_deduplicates_without_send") { return; }
    let (root, repository) = queue_repository("img-direct-confirmed");
    let mut command = image_command("cutex.native-direct-confirmed", &root, "confirmed-image");
    command.params.strategy = UserInputStrategy::Auto;
    repository.register_identity(&command.cutex_session_id, &command.params).unwrap();
    let prior = InputDelivery::Unknown { thread_id:command.thread_id.clone(), since:(Utc::now()-chrono::Duration::days(1)).to_rfc3339() };
    repository.set_delivery(&command.cutex_session_id, "confirmed-image", None, Some(prior)).unwrap();
    let result = submit_v2_with_repository_and_image_settings(command, &repository, ImageInputSettings::default(),
        |_, _| Some(None), |_, _| panic!("existing identity"),
        thread_with(json!([{"id":"accepted-turn","status":"completed","items":[{"type":"userMessage","id":"image-item","clientId":"confirmed-image","content":[]}]}])),
        never_sent, no_flush).unwrap();
    assert_eq!(result.disposition, UserInputDisposition::Deduplicated);
    assert_eq!(result.turn_id.as_deref(), Some("accepted-turn"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn image_persisted_host_settings_reload_without_runtime_restart() {
    if test_runs_in_private_home("image_persisted_host_settings_reload_without_runtime_restart") { return; }
    use cutex::config::store::{load_codez_config, save_codez_config};
    let mut config = load_codez_config();
    assert_eq!(config.image_input, ImageInputSettings::default());
    let (root, repository) = queue_repository("img-persist");
    let mut command = image_command("cutex.native-persist", &root, "persist-steer");
    command.params.strategy = UserInputStrategy::Auto;
    for behavior in [ImageBusyBehavior::Steer, ImageBusyBehavior::Queue] {
        config.image_input.busy_behavior = behavior;
        save_codez_config(&config).unwrap();
        let loaded = load_codez_config().image_input;
        assert_eq!(loaded.busy_behavior, behavior);
        let calls = std::cell::RefCell::new(Vec::new());
        let result = route_submit_with_image_settings(&command, &repository, loaded,
            Some("running".into()), true, &recording_sender(&calls), &no_flush).unwrap();
        assert_eq!(result.disposition, if behavior == ImageBusyBehavior::Steer {
            UserInputDisposition::Steered
        } else { UserInputDisposition::Queued });
    }
    std::fs::remove_dir_all(root).unwrap();
}

fn image_claude_fixture() -> String {
    let session = format!("cutex.{}", uuid::Uuid::new_v4());
    let private = std::path::PathBuf::from(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").unwrap());
    let mut record = cutex::session::model::CutexSessionRecord::new(
        session.clone(), None, current_host_name(), private.display().to_string(), None).unwrap();
    record.claude_session = Some(serde_json::from_value(json!({
        "deployment": {"python":private.join("python.exe"), "adapter_source":&private,
            "claude_cli":private.join("claude.exe"), "claude_home":&private, "tui_binary":private.join("tui.exe")},
        "thread_id":"thread-q", "state_dir":private.join("state"), "model":"opus"
    })).unwrap());
    let mut store = cutex::session::store::load_cutex_session_store().unwrap();
    store.sessions.insert(session.clone(), record);
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    session
}

fn image_command(session: &str, root: &std::path::Path, client: &str) -> UserInputSubmitCommand {
    let mut command = queue_command(session, client);
    for name in ["one 图片 [x].png", "two.png"] {
        let path = root.join(name);
        std::fs::write(&path, b"image fixture").unwrap();
        command.params.input.push(json!({"type":"localImage", "path":path, "detail":"original"}));
    }
    command
}

#[test]
fn image_default_queue_preserves_original_and_flushes_original_for_both_runtimes() {
    if test_runs_in_private_home("image_default_queue_preserves_original_and_flushes_original_for_both_runtimes") { return; }
    for session in ["cutex.native-image".into(), image_claude_fixture()] {
        let (root, repository) = queue_repository("img-default");
        let mut command = image_command(&session, &root, "image-default");
        command.params.strategy = UserInputStrategy::Auto;
        let result = route_submit(&command, &repository, Some("running".into()), true, &never_sent, &no_flush).unwrap();
        assert_eq!(result.disposition, UserInputDisposition::Queued);
        assert_eq!(repository.list(&session).unwrap().1[0].input, command.params.input);
        let events = std::cell::RefCell::new(Vec::new());
        let calls = std::cell::Cell::new(0);
        assert!(flush_queued_locked(&session, false, &repository, None, idle_thread,
            |cmd, method, params, disposition| {
                assert_eq!(method, "turn/start");
                assert_eq!(params["input"], json!(command.params.input));
                calls.set(calls.get() + 1);
                accepted("next-turn")(cmd, method, params, disposition)
            }, recording_events(&events)).unwrap());
        assert_eq!(calls.get(), 1);
        assert!(repository.list(&session).unwrap().1.is_empty());
        assert!(events.borrow().iter().any(|event| event == "cutex/userInput/queueRemoved"));
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn image_explicit_steer_converts_only_claude_and_waits_for_acceptance() {
    if test_runs_in_private_home("image_explicit_steer_converts_only_claude_and_waits_for_acceptance") { return; }
    for (session, claude) in [("cutex.native-image".into(), false), (image_claude_fixture(), true)] {
        let (root, repository) = queue_repository("img-steer");
        let command = image_command(&session, &root, "image-steer");
        repository.enqueue(&session, &command.thread_id, &command.management_request_id, &command.params).unwrap();
        let original = repository.list(&session).unwrap();
        let result = steer_queued_with_repository(&command, false, &repository, Some("running".into()), false, || true,
            |cmd, method, params, disposition| {
                assert_eq!(method, "turn/steer");
                assert_eq!(params["expectedTurnId"], "running");
                assert_eq!(cmd.params.input, command.params.input);
                assert_eq!(repository.list(&session).unwrap().1[0].input, command.params.input);
                assert_eq!(params["input"][0], command.params.input[0]);
                if claude {
                    assert_eq!(params["input"][1]["type"], "text");
                    assert_eq!(params["input"][2]["type"], "text");
                    assert!(params["input"][1]["text"].as_str().unwrap().contains("JSON string:"));
                } else { assert_eq!(params["input"], json!(command.params.input)); }
                accepted("running")(cmd, method, params, disposition)
            }).unwrap();
        assert!(result.app_server_accepted);
        assert_eq!(result.disposition, UserInputDisposition::Steered);
        assert_eq!(image_steering::accepted_delivery(&command.params.input, claude, &result),
            Some(if claude { "pathText" } else { "nativeImage" }));
        let mut not_accepted = result.clone();
        not_accepted.app_server_accepted = false;
        assert_eq!(image_steering::accepted_delivery(&command.params.input, claude, &not_accepted), None);
        let mut deduplicated = result.clone();
        deduplicated.disposition = UserInputDisposition::Deduplicated;
        assert_eq!(image_steering::accepted_delivery(&command.params.input, claude, &deduplicated), None);
        assert_eq!(repository.list(&session).unwrap(), original, "queue_order alone removes after confirmed success");
        assert!(matches!(repository.delivery(&session, "image-steer").unwrap(), Some(InputDelivery::Accepted { .. })));
        // A repeat insertion cannot send the same identity again.
        assert!(steer_queued_with_repository(&command, false, &repository, Some("running".into()), false,
            || true, never_sent).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn image_missing_file_failure_retry_unknown_and_owner_stop_preserve_queue() {
    if test_runs_in_private_home("image_missing_file_failure_retry_unknown_and_owner_stop_preserve_queue") { return; }
    let session = image_claude_fixture();
    let (root, repository) = queue_repository("img-failure");
    let command = image_command(&session, &root, "image-failure");
    repository.enqueue(&session, &command.thread_id, &command.management_request_id, &command.params).unwrap();
    let original = repository.list(&session).unwrap();
    let path = command.params.input[2]["path"].as_str().unwrap();
    std::fs::remove_file(path).unwrap();
    let error = steer_queued_with_repository(&command, false, &repository, Some("running".into()), false,
        || true, never_sent).unwrap_err();
    assert_eq!(error.details["reason"], "imageUnavailable");
    assert_eq!(repository.list(&session).unwrap(), original);
    assert!(repository.delivery(&session, "image-failure").unwrap().is_none());
    std::fs::write(path, b"restored image").unwrap();
    let refused = |_: &UserInputSubmitCommand, _: &str, params: Value, _: UserInputDisposition| {
        assert_eq!(params["input"][1]["type"], "text");
        Err(native_user_input_failure(&json!({"code":-32000,"message":"guarded phase"})))
    };
    let error = steer_queued_with_repository(&command, false, &repository, Some("running".into()), false,
        || true, refused).unwrap_err();
    assert_eq!(error.details["reason"], "runtimeRefused");
    assert!(repository.delivery(&session, "image-failure").unwrap().is_none());
    assert_eq!(repository.list(&session).unwrap(), original);
    let error = steer_queued_with_repository(&command, false, &repository, Some("running".into()), false,
        || true, |_,_,_,_| Err(outcome_unknown_error())).unwrap_err();
    assert!(error.outcome_unknown);
    // Even removing the file after an uncertain send must not claim a definite refusal.
    std::fs::remove_file(path).unwrap();
    let retry = steer_queued_with_repository(&command, false, &repository, Some("running".into()), false,
        || true, never_sent).unwrap_err();
    assert_eq!(retry.code, "user_input_in_progress");
    repository.pause(&session, "ownerStop", Some("running".into())).unwrap();
    assert!(!flush_queued_locked(&session, false, &repository, None, idle_thread, never_sent,
        |_,_,_,_| panic!("no success event")).unwrap());
    let queue = repository.list(&session).unwrap().1;
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].input, command.params.input);
    assert_eq!(queue[0].delivery.as_deref(), Some("unknown"));
    drop(repository);
    let reopened = UserInputRepository::open(&root).unwrap();
    assert_eq!(reopened.list(&session).unwrap().1[0].input, command.params.input);
    assert!(matches!(reopened.delivery(&session, "image-failure").unwrap(), Some(InputDelivery::Unknown { .. })));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn image_steer_policy_is_opt_in_and_explicit_queue_and_idle_still_keep_original() {
    let (root, repository) = queue_repository("img-policy");
    let mut command = image_command("cutex.native-policy", &root, "policy");
    let settings: ImageInputSettings = serde_json::from_str(r#"{"busyBehavior":"steer"}"#).unwrap();
    command.params.strategy = UserInputStrategy::Auto;
    let calls = std::cell::RefCell::new(Vec::new());
    let result = route_submit_with_image_settings(&command, &repository, settings,
        Some("running".into()), true, &recording_sender(&calls), &no_flush).unwrap();
    assert_eq!(result.disposition, UserInputDisposition::Steered);
    command.params.strategy = UserInputStrategy::Queue;
    let queued = route_submit_with_image_settings(&command, &repository, settings,
        Some("running".into()), true, &never_sent, &no_flush).unwrap();
    assert_eq!(queued.disposition, UserInputDisposition::Queued);
    let params = json!({"input":command.params.input});
    assert_eq!(prepare_native_input(true, "turn/start", params.clone()).unwrap(), params);
    assert_eq!(prepare_native_input(false, "turn/steer", params.clone()).unwrap(), params);
    std::fs::remove_dir_all(root).unwrap();
}
