use super::*;

fn source(root: &Path) -> CutexSessionRecord {
    let mut record = CutexSessionRecord::new(
        "cutex.source".into(),
        None,
        "test-host".into(),
        root.to_string_lossy().into(),
        Some("claude-profile".into()),
    )
    .unwrap();
    record.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
    record.runtime_pid = Some(99999);
    record.runtime_generation = 7;
    record.current_runtime_agent_id = Some("source-runtime".into());
    record.agent_enabled = true;
    record.formal_agent_name = Some("Source".into());
    record.model_defaults = Some("sonnet".into());
    record.claude_session = Some(ClaudeSession {
        deployment: ClaudeDeployment {
            python: root.join("python"),
            adapter_source: root.join("adapter"),
            claude_cli: root.join("claude"),
            claude_home: root.join("account"),
            tui_binary: root.join("tui"),
        },
        thread_id: uuid::Uuid::new_v4().to_string(),
        state_dir: root.join("source-state"),
        model: "sonnet".into(),
        effort: None,
        instructions: "Project context".into(),
        allow_write: true,
        allow_exec: false, write_explicit: false, exec_explicit: false,
        native_session_id: Some(uuid::Uuid::new_v4().to_string()),
        connection_id: Some("source-connection".into()),
        initial_permission_mode: None,
    });
    record
}

#[test]
fn fork_projection_preserves_account_but_never_copies_runtime_ownership() {
    // runtime_dir() is read here and again inside fork_record: HOME must not move in between.
    let _home = crate::config::paths::test_home_env_lock();
    let id = format!("cutex.{}", uuid::Uuid::new_v4());
    let target = crate::config::paths::runtime_dir()
        .unwrap()
        .join("claude/sessions")
        .join(&id);
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("state.sqlite3"), b"fixture").unwrap();
    let source = source(&target);
    let thread = uuid::Uuid::new_v4().to_string();
    let native = uuid::Uuid::new_v4().to_string();
    let facts = json!({"threadId": thread, "nativeSessionId": native, "stateDir": target});
    let fork = fork_record(&source, &id, "Fork", &facts).unwrap();
    assert_eq!(
        (
            fork.runtime_pid,
            fork.runtime_generation,
            fork.current_runtime_agent_id
        ),
        (None, 0, None)
    );
    assert!(fork.explicit_launch.is_none());
    assert_eq!(fork.profile, source.profile);
    let binding = fork.claude_session.unwrap();
    let original = source.claude_session.as_ref().unwrap();
    assert_eq!(binding.deployment, original.deployment);
    assert_eq!(
        (
            binding.thread_id,
            binding.native_session_id,
            binding.connection_id
        ),
        (thread, Some(native), None)
    );
    let mut invalid = facts;
    invalid["threadId"] = json!(original.thread_id);
    assert!(fork_record(&source, &id, "Fork", &invalid).is_err());
    std::fs::remove_dir_all(target).unwrap();
}

#[test]
fn managed_skill_catalog_is_project_and_account_scoped() {
    let root = std::env::temp_dir().join(format!("claude-skills-{}", uuid::Uuid::new_v4()));
    for child in [
        ".agents/skills",
        ".claude/skills",
        "account/cutex-skills",
        "account/skills",
    ] {
        std::fs::create_dir_all(root.join(child)).unwrap();
    }
    let record = source(&root);
    let roots = managed_skill_roots_with(&record, None).unwrap();
    assert_eq!(
        roots,
        [".agents/skills", ".claude/skills", "account/cutex-skills"]
            .map(|p| PathBuf::from(crate::agent_management::strip_verbatim_prefix(
                root.join(p).canonicalize().unwrap().to_str().unwrap())))
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn user_level_skill_root_follows_the_account_catalog() {
    let root = std::env::temp_dir().join(format!("claude-skills-user-{}", uuid::Uuid::new_v4()));
    for child in [".agents/skills", "account/cutex-skills", "user-skills"] {
        std::fs::create_dir_all(root.join(child)).unwrap();
    }
    let record = source(&root);
    let public = |p: &str| PathBuf::from(crate::agent_management::strip_verbatim_prefix(
        root.join(p).canonicalize().unwrap().to_str().unwrap()));
    let roots = managed_skill_roots_with(&record, Some(&root.join("user-skills"))).unwrap();
    assert_eq!(roots, [".agents/skills", "account/cutex-skills", "user-skills"].map(public));
    // Missing user directory: skipped, not an error.
    let roots = managed_skill_roots_with(&record, Some(&root.join("missing"))).unwrap();
    assert_eq!(roots, [".agents/skills", "account/cutex-skills"].map(public));
    assert!(user_skills_root().unwrap().ends_with(USER_SKILLS_DIR));
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(windows)]
#[test]
fn managed_skill_roots_use_normal_windows_paths() {
    let root = std::env::temp_dir().join(format!("claude-skills-win-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join(".agents/skills/example")).unwrap();
    std::fs::create_dir_all(root.join("account")).unwrap();
    std::fs::write(root.join(".agents/skills/example/SKILL.md"), "# Example").unwrap();
    let roots = managed_skill_roots(&source(&root)).unwrap();
    assert_eq!(roots.len(), 1);
    let plain_workspace = crate::agent_management::strip_verbatim_prefix(root.canonicalize().unwrap().to_str().unwrap());
    assert!(!roots[0].to_str().unwrap().starts_with(r"\\?\"));
    assert_eq!(roots[0], Path::new(&plain_workspace).join(".agents/skills"));
    assert_eq!(std::fs::read_to_string(roots[0].join("example/SKILL.md")).unwrap(), "# Example");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn plain_tui_revert_is_not_a_controller_structural_method() {
    // Rejected before loading a session or contacting an adapter.
    let error = request("not-a-session", "thread/revert", json!({})).unwrap_err();
    assert!(error.to_string().contains("unsupported structural operation"));
}

#[test]
fn escaped_managed_skill_root_is_skipped_without_blocking_valid_roots() {
    let root = std::env::temp_dir().join(format!("claude-skills-escape-{}", uuid::Uuid::new_v4()));
    let workspace = root.join("workspace");
    let outside = root.join("outside");
    for path in [workspace.join(".agents/skills"), workspace.join(".claude"), workspace.join("account"), outside.clone()] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(outside.join("sentinel"), "outside").unwrap();
    let link = workspace.join(".claude/skills");
    crate::platform::shared_assets::link_directory(&outside, &link).unwrap();
    let roots = managed_skill_roots(&source(&workspace)).unwrap();
    assert_eq!(roots.len(), 1);
    assert!(roots[0].ends_with(".agents/skills"));
    #[cfg(windows)]
    std::fs::remove_dir(&link).unwrap();
    #[cfg(unix)]
    std::fs::remove_file(&link).unwrap();
    assert_eq!(std::fs::read_to_string(outside.join("sentinel")).unwrap(), "outside");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn uncertain_temporary_adapter_has_a_durable_owner_without_process_termination() {
    use crate::app_server::client::AppServerClientError;
    let root = std::env::temp_dir().join(format!("claude-structural-owner-{}", uuid::Uuid::new_v4()));
    let record = source(&root);
    let session = record.claude_session.as_ref().unwrap();
    std::fs::create_dir_all(&session.state_dir).unwrap();
    let ready = Ready { endpoint: "ws://127.0.0.1:1".into(), runtime_id: "private-adapter".into(), pid: std::process::id() };
    let mut owner = temporary_owner(&record.cutex_session_id, session, &ready, "claude/thread/revert");
    save_temporary_owner(session, &owner).unwrap();
    finish_temporary_owner(session, &mut owner, "outcome_unknown");
    let saved: Value = serde_json::from_slice(&std::fs::read(session.state_dir.join(TEMPORARY_OWNER_FILE)).unwrap()).unwrap();
    assert_eq!(saved["adapterPid"], std::process::id());
    assert_eq!(saved["runtimeId"], "private-adapter");
    assert_eq!(saved["state"], "outcome_unknown");
    assert!(saved["operationId"].as_str().is_some());
    assert!(crate::platform::process::process_is_running(std::process::id()));
    for error in [AppServerClientError::Timeout { method: "claude/thread/revert".into() },
        AppServerClientError::Disconnected("lost response".into()), AppServerClientError::Transport("read failed".into())] {
        assert!(uncertain_request(&error));
    }
    assert!(!uncertain_request(&AppServerClientError::Connect("not sent".into())));
    assert!(!uncertain_request(&AppServerClientError::Rpc(crate::app_server::protocol::RpcError {
        code: -32602, message: "invalid request".into(), data: None,
    })));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn history_restart_updates_identity_and_fences_stale_results() {
    let mut current = source(Path::new("fixture"));
    let expected = current.claude_session.clone().unwrap();
    let sid = uuid::Uuid::new_v4().to_string();
    apply_binding(&mut current, &expected, Some(&sid), Some("new-connection")).unwrap();
    assert_eq!(current.runtime_generation, 8);
    assert_eq!(
        current.current_runtime_agent_id.as_deref(),
        Some("claude.new-connection")
    );
    assert_eq!(
        current
            .claude_session
            .as_ref()
            .unwrap()
            .native_session_id
            .as_deref(),
        Some(sid.as_str())
    );
    assert!(apply_binding(&mut current, &expected, Some("stale"), Some("stale")).is_err());
    let committed = current.claude_session.clone().unwrap();
    apply_binding(&mut current, &committed, Some(&sid), Some("new-connection")).unwrap();
    assert_eq!(current.runtime_generation, 8);
    current.runtime_generation = u64::MAX;
    assert!(apply_binding(&mut current, &committed, Some("uncommitted"), Some("next")).is_err());
    assert_eq!(current.claude_session.as_ref(), Some(&committed));
}
