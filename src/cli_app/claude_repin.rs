//! `cutex human claude repin`: move Claude sessions to another adapter
//! runtime. The rules live in `cutex::runtime::claude_pin`.
use std::path::Path;

use cutex::runtime::claude::ClaudeDeployment;
use cutex::runtime::claude_pin::{repin, repin_all};

pub(super) fn run(
    id: Option<&str>,
    all: bool,
    deployment: Option<&Path>,
    dry_run: bool,
) -> anyhow::Result<serde_json::Value> {
    use anyhow::Context;
    let target = match deployment {
        Some(file) => {
            let target: ClaudeDeployment = serde_json::from_slice(
                &std::fs::read(file).with_context(|| format!("read {}", file.display()))?,
            )
            .with_context(|| format!("{} is not a Claude deployment", file.display()))?;
            target.validate()?;
            target
        }
        None => ClaudeDeployment::selected()?,
    };
    let sessions = match (id, all) {
        (Some(id), false) => vec![repin(id, &target, dry_run)?],
        (None, true) => repin_all(&target, dry_run)?,
        _ => anyhow::bail!("name one Claude session or pass --all"),
    };
    Ok(serde_json::json!({"dryRun": dry_run, "target": target, "sessions": sessions}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutex::runtime::claude_pin::{apply_pending_at_start, PENDING_FILE, PREVIOUS_FILE};
    use std::path::PathBuf;

    fn install_runtime(root: &Path, name: &str, home: &Path) -> (ClaudeDeployment, PathBuf) {
        let rt = root.join(name);
        let adapter = rt.join("adapter");
        std::fs::create_dir_all(adapter.join("src/cute_claude")).unwrap();
        std::fs::create_dir_all(adapter.join("scripts")).unwrap();
        for file in ["src/cute_claude/__main__.py", "scripts/connect_tui.py"] {
            std::fs::write(adapter.join(file), "").unwrap();
        }
        let binary = rt.join("python");
        std::fs::write(&binary, "").unwrap();
        let deployment = ClaudeDeployment {
            python: binary.clone(),
            adapter_source: adapter,
            claude_cli: root.join("claude"),
            claude_home: home.to_path_buf(),
            tui_binary: root.join("tui"),
        };
        let file = rt.join("deployment.json");
        std::fs::write(&file, serde_json::to_vec(&deployment).unwrap()).unwrap();
        (deployment, file)
    }

    fn pin(id: &str) -> ClaudeDeployment {
        cutex::session::store::load_cutex_session_store().unwrap().sessions[id]
            .claude_session
            .clone()
            .unwrap()
            .deployment
    }

    fn set_runtime_pid(id: &str, pid: Option<u32>) {
        let mut store = cutex::session::store::load_cutex_session_store().unwrap();
        store.sessions.get_mut(id).unwrap().runtime_pid = pid;
        cutex::session::store::save_cutex_session_store(&store).unwrap();
    }

    struct Fixture {
        home: super::super::test_home::IsolatedTestHome,
        r3: ClaudeDeployment,
        r3_file: PathBuf,
        r4: ClaudeDeployment,
        r4_file: PathBuf,
    }

    fn fixture(prefix: &str) -> Fixture {
        let home = super::super::test_home::IsolatedTestHome::new(prefix).unwrap();
        let root = home.root().to_path_buf();
        for file in ["claude", "tui"] {
            std::fs::write(root.join(file), "").unwrap();
        }
        let login = root.join("shared-login");
        std::fs::create_dir(&login).unwrap();
        let (r3, r3_file) = install_runtime(&root, "claude-r3", &login);
        let (r4, r4_file) = install_runtime(&root, "claude-r4", &login);
        r3.install().unwrap();
        Fixture { home, r3, r3_file, r4, r4_file }
    }

    fn set_state_schema(state_dir: &Path, schema: &str) {
        std::fs::create_dir_all(state_dir).unwrap();
        let db = rusqlite::Connection::open(state_dir.join("state.sqlite3")).unwrap();
        db.execute_batch("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL)").unwrap();
        db.execute("INSERT OR REPLACE INTO meta VALUES('schema',?)", [schema]).unwrap();
    }

    #[test]
    fn schema_two_repin_refuses_legacy_without_changing_pin_or_state() {
        let f = fixture("claude-repin-schema-two");
        let record = cutex::runtime::claude::create("schema-two", f.home.root(), "sonnet", false, false).unwrap();
        let session = record.claude_session.as_ref().unwrap();
        set_state_schema(&session.state_dir, "2");
        let original = std::fs::read(session.state_dir.join("state.sqlite3")).unwrap();
        for dry_run in [true, false] {
            let result = run(Some(&record.cutex_session_id), false, Some(&f.r4_file), dry_run).unwrap();
            assert_eq!(result["sessions"][0]["outcome"], "skipped");
            assert!(result["sessions"][0]["reason"].as_str().unwrap().contains("restore the pre-r8"));
            assert_eq!(pin(&record.cutex_session_id), f.r3);
            assert_eq!(std::fs::read(session.state_dir.join("state.sqlite3")).unwrap(), original);
            assert!(!session.state_dir.join(PENDING_FILE).exists());
            assert!(!session.state_dir.join(PREVIOUS_FILE).exists());
        }
        std::fs::write(f.r4.adapter_source.join("src/cute_claude/features.json"), r#"{"release":"claude-r8","features":{}}"#).unwrap();
        assert_eq!(run(Some(&record.cutex_session_id), false, Some(&f.r4_file), false).unwrap()["sessions"][0]["outcome"], "applied");
        assert_eq!(pin(&record.cutex_session_id), f.r4);
    }

    #[test]
    fn pending_downgrade_is_rechecked_after_state_migrates() {
        let f = fixture("claude-repin-schema-pending");
        std::fs::write(f.r3.adapter_source.join("src/cute_claude/features.json"), r#"{"release":"claude-r8","features":{}}"#).unwrap();
        let record = cutex::runtime::claude::create("pending-schema", f.home.root(), "sonnet", false, false).unwrap();
        let session = record.claude_session.as_ref().unwrap();
        set_state_schema(&session.state_dir, "1");
        set_runtime_pid(&record.cutex_session_id, Some(std::process::id()));
        assert_eq!(run(Some(&record.cutex_session_id), false, Some(&f.r4_file), false).unwrap()["sessions"][0]["outcome"], "deferred");
        set_state_schema(&session.state_dir, "2");
        set_runtime_pid(&record.cutex_session_id, None);
        let current = cutex::session::store::load_cutex_session_store().unwrap().sessions[&record.cutex_session_id].clone();
        let kept = apply_pending_at_start(current).unwrap();
        assert_eq!(kept.claude_session.unwrap().deployment, f.r3);
        assert_eq!(pin(&record.cutex_session_id), f.r3);
        assert!(!session.state_dir.join(PREVIOUS_FILE).exists());
        assert!(session.state_dir.join(PENDING_FILE).exists());
    }

    #[test]
    fn offline_sessions_move_now_and_undo_with_the_saved_previous_pin() {
        let f = fixture("claude-repin-offline");
        let a = cutex::runtime::claude::create("alpha", f.home.root(), "sonnet", false, false).unwrap();
        let b = cutex::runtime::claude::create("beta", f.home.root(), "sonnet", false, false).unwrap();
        let (a, b) = (a.cutex_session_id, b.cutex_session_id);
        f.r4.install().unwrap();

        // Dry run reports and changes nothing.
        let dry = run(None, true, None, true).unwrap();
        assert_eq!(dry["sessions"].as_array().unwrap().len(), 2);
        assert!(dry["sessions"].as_array().unwrap().iter().all(|s| s["outcome"] == "applied"));
        assert_eq!(pin(&a), f.r3);

        // Default target = the selected runtime (r4); the thread and state stay.
        let before = cutex::session::store::load_cutex_session_store().unwrap().sessions[&a]
            .claude_session.clone().unwrap();
        let moved = run(Some(&a), false, None, false).unwrap();
        let report = &moved["sessions"][0];
        assert_eq!(report["outcome"], "applied");
        assert_eq!(report["previous"]["adapter_source"], serde_json::json!(f.r3.adapter_source));
        let after = cutex::session::store::load_cutex_session_store().unwrap().sessions[&a]
            .claude_session.clone().unwrap();
        assert_eq!(after.deployment, f.r4);
        assert_eq!((&after.thread_id, &after.state_dir, &after.native_session_id),
            (&before.thread_id, &before.state_dir, &before.native_session_id));
        assert_eq!(pin(&b), f.r3, "only the named session moves");

        // Running it again is a no-op; --all moves the rest.
        assert_eq!(run(Some(&a), false, None, false).unwrap()["sessions"][0]["outcome"], "unchanged");
        let all = run(None, true, None, false).unwrap();
        let outcomes: Vec<_> = all["sessions"].as_array().unwrap().iter()
            .map(|s| s["outcome"].as_str().unwrap().to_string()).collect();
        let mut expected = vec![("unchanged", &a), ("applied", &b)];
        expected.sort_by(|x, y| x.1.cmp(y.1));
        assert_eq!(outcomes, expected.iter().map(|e| e.0.to_string()).collect::<Vec<_>>());

        // Undo: the saved previous pin is a valid target.
        let previous = after.state_dir.join(PREVIOUS_FILE);
        let undo = run(Some(&a), false, Some(&previous), false).unwrap();
        assert_eq!(undo["sessions"][0]["outcome"], "applied");
        assert_eq!(pin(&a), f.r3);
        // An explicit file target works the same way.
        run(Some(&a), false, Some(&f.r4_file), false).unwrap();
        assert_eq!(pin(&a), f.r4);
        let _ = &f.r3_file;
    }

    #[test]
    fn online_sessions_defer_to_the_next_fresh_start() {
        let f = fixture("claude-repin-online");
        let s = cutex::runtime::claude::create("gamma", f.home.root(), "sonnet", false, false).unwrap();
        let id = s.cutex_session_id;
        // A live adapter pid (this test process) counts as online.
        set_runtime_pid(&id, Some(std::process::id()));
        let report = run(Some(&id), false, Some(&f.r4_file), false).unwrap();
        assert_eq!(report["sessions"][0]["outcome"], "deferred");
        assert_eq!(pin(&id), f.r3, "an online session keeps its pin");
        let state_dir = s.claude_session.unwrap().state_dir;
        assert!(state_dir.join(PENDING_FILE).is_file());

        let record = || cutex::session::store::load_cutex_session_store().unwrap().sessions[&id].clone();
        // Still online at Start (adapter reused): nothing applied.
        assert_eq!(apply_pending_at_start(record()).unwrap().claude_session.unwrap().deployment, f.r3);
        // Offline: the next Start applies it and clears the pending file.
        set_runtime_pid(&id, None);
        let started = apply_pending_at_start(record()).unwrap();
        assert_eq!(started.claude_session.unwrap().deployment, f.r4);
        assert_eq!(pin(&id), f.r4);
        assert!(!state_dir.join(PENDING_FILE).exists());
        assert!(state_dir.join(PREVIOUS_FILE).is_file());

        // Offline now: a repeated request applies directly and clears the pending file.
        set_runtime_pid(&id, Some(std::process::id()));
        run(Some(&id), false, Some(&f.r3_file), false).unwrap();
        assert!(state_dir.join(PENDING_FILE).is_file());
        set_runtime_pid(&id, None);
        assert_eq!(run(Some(&id), false, Some(&f.r3_file), false).unwrap()["sessions"][0]["outcome"], "applied");
        assert_eq!(pin(&id), f.r3);
        assert!(!state_dir.join(PENDING_FILE).exists());

        // A pending change whose session was re-pinned by other means is stale: dropped.
        set_runtime_pid(&id, Some(std::process::id()));
        run(Some(&id), false, Some(&f.r4_file), false).unwrap();
        let mut store = cutex::session::store::load_cutex_session_store().unwrap();
        let other = store.sessions.get_mut(&id).unwrap();
        other.runtime_pid = None;
        other.claude_session.as_mut().unwrap().deployment.tui_binary = f.home.root().join("claude");
        cutex::session::store::save_cutex_session_store(&store).unwrap();
        let kept = apply_pending_at_start(record()).unwrap();
        assert_eq!(kept.claude_session.unwrap().deployment.adapter_source, f.r3.adapter_source);
        assert!(!state_dir.join(PENDING_FILE).exists());
        run(Some(&id), false, Some(&f.r3_file), false).unwrap();
        assert_eq!(pin(&id), f.r3);

        // Re-pinning an online session to its current runtime cancels a pending change.
        set_runtime_pid(&id, Some(std::process::id()));
        run(Some(&id), false, Some(&f.r4_file), false).unwrap();
        assert!(state_dir.join(PENDING_FILE).is_file());
        let cancel = run(Some(&id), false, Some(&f.r3_file), false).unwrap();
        assert_eq!(cancel["sessions"][0]["outcome"], "unchanged");
        assert!(!state_dir.join(PENDING_FILE).exists());
    }

    #[test]
    fn another_claude_home_is_refused() {
        let f = fixture("claude-repin-home");
        let s = cutex::runtime::claude::create("delta", f.home.root(), "sonnet", false, false).unwrap();
        let other = f.home.root().join("other-login");
        std::fs::create_dir(&other).unwrap();
        let mut elsewhere = f.r4.clone();
        elsewhere.claude_home = other;
        let file = f.home.root().join("elsewhere.json");
        std::fs::write(&file, serde_json::to_vec(&elsewhere).unwrap()).unwrap();
        let report = run(Some(&s.cutex_session_id), false, Some(&file), false).unwrap();
        assert_eq!(report["sessions"][0]["outcome"], "skipped");
        assert!(report["sessions"][0]["reason"].as_str().unwrap().contains("Claude config home"));
        assert_eq!(pin(&s.cutex_session_id), f.r3);
    }
}
