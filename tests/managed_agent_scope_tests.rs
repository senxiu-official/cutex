#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::process::ExitStatusExt;
    use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use cutex::launch::command::LaunchCommand;
    use cutex::platform::command::command_exists_in_path;
    use cutex::runtime::lifecycle::spawn_managed_agent_runtime_launch;
    use cutex::runtime::process_scope::{
        managed_agent_process_isolation, managed_agent_scope_control_group,
        managed_agent_scope_is_absent, terminate_managed_agent_scope, ManagedAgentProcessIsolation,
    };
    use uuid::Uuid;

    #[test]
    fn scope_timeout_can_be_reobserved_absent_after_child_finishes() {
        let id = format!("cutex.scope-timeout-proof-{}", Uuid::new_v4().simple());
        if let ManagedAgentProcessIsolation::Direct { reason } =
            managed_agent_process_isolation(&id)
        {
            eprintln!("SKIPPED real scope timeout proof: {reason}");
            return;
        }
        let root = std::env::temp_dir().join(format!(
            "cutex-scope-timeout-proof-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir(&root).unwrap();
        let log = root.join("child.log");
        let launch = LaunchCommand::new("sh").args([
            "-c",
            "trap 'sleep 3; exit 0' TERM; printf 'ready\\n'; while :; do sleep 1; done",
        ]);
        let mut child =
            spawn_managed_agent_runtime_launch(&id, &launch, root.to_str().unwrap(), &log).unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| {
            wait_for_log(&log, "ready", Duration::from_secs(5));
            assert!(!managed_agent_scope_is_absent(&id).unwrap());
            let timeout = terminate_managed_agent_scope(&id, false).unwrap();
            assert_eq!(timeout.detail, "scope_terminate_timeout");
            assert!(!timeout.stopped);
            child.wait().unwrap();
            assert!(managed_agent_scope_is_absent(&id).unwrap());
            assert!(managed_agent_scope_is_absent(&id).unwrap());
        }));
        let _ = terminate_managed_agent_scope(&id, true);
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        fs::remove_dir_all(root).unwrap();
        if let Err(error) = result {
            resume_unwind(error);
        }
    }

    #[test]
    fn managed_agent_scope_isolates_crashes_and_preserves_lifecycle() {
        let cutex_session_id = format!("cutex.scope-integration-{}", Uuid::new_v4().simple());
        let unit_name = match managed_agent_process_isolation(&cutex_session_id) {
            ManagedAgentProcessIsolation::SystemdScope { unit_name } => unit_name,
            ManagedAgentProcessIsolation::Direct { reason } => {
                eprintln!("skipped real systemd scope boundary: {reason}");
                return;
            }
        };
        let test_dir = std::env::temp_dir().join(format!(
            "cutex-managed-agent-scope-test-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&test_dir).expect("create scope test directory");
        let log_path = test_dir.join("runtime.log");
        let launch = LaunchCommand::new("sh").args([
            "-c",
            "trap '' TERM; sleep 30 & descendant=$!; printf 'root_pid=%s\\n' \"$$\"; printf 'self_cgroup='; tr '\\n' ' ' < /proc/self/cgroup; printf '\\nchild_cgroup='; tr '\\n' ' ' < \"/proc/$descendant/cgroup\"; printf '\\nready\\n'; wait",
        ]);
        let mut child = spawn_managed_agent_runtime_launch(
            &cutex_session_id,
            &launch,
            test_dir.to_str().expect("test path is UTF-8"),
            &log_path,
        )
        .expect("spawn scoped runtime");
        let mut restarted_child = None;

        let result = catch_unwind(AssertUnwindSafe(|| {
            let log = wait_for_log(&log_path, "ready", Duration::from_secs(5));
            let root_pid = log_field(&log, "root_pid=")
                .parse::<u32>()
                .expect("root PID is numeric");
            assert_eq!(
                root_pid,
                child.id(),
                "systemd scope wrapper must exec in place"
            );

            let self_cgroup = log_field(&log, "self_cgroup=");
            let child_cgroup = log_field(&log, "child_cgroup=");
            assert_eq!(self_cgroup, child_cgroup);
            assert!(
                self_cgroup.contains(&format!("/{unit_name}")),
                "runtime cgroup did not contain its Agent unit: {self_cgroup}"
            );
            assert!(!self_cgroup.contains("cutex-agent-bus.service"));

            let parent_cgroup = fs::read_to_string("/proc/self/cgroup")
                .expect("read test process cgroup")
                .replace('\n', " ");
            assert_ne!(self_cgroup, parent_cgroup.trim());

            let control_group = managed_agent_scope_control_group(&cutex_session_id)
                .expect("query Agent scope")
                .expect("Agent scope is populated");
            assert!(self_cgroup.contains(&control_group));
            let scope_memory = cgroup_metric_path(&control_group, "memory.current");
            let parent_memory = cgroup_metric_path(
                unified_cgroup(&parent_cgroup).expect("parent unified cgroup"),
                "memory.current",
            );
            let scope_metadata = fs::metadata(&scope_memory).expect("scope memory counter exists");
            let parent_metadata =
                fs::metadata(&parent_memory).expect("parent memory counter exists");
            assert_ne!(
                (scope_metadata.dev(), scope_metadata.ino()),
                (parent_metadata.dev(), parent_metadata.ino()),
                "Agent and service must not share a resource counter"
            );
            assert!(
                fs::read_to_string(&scope_memory)
                    .expect("read scope memory counter")
                    .trim()
                    .parse::<u64>()
                    .expect("scope memory counter is numeric")
                    > 0
            );

            let graceful = terminate_managed_agent_scope(&cutex_session_id, false)
                .expect("attempt graceful Agent scope close");
            assert!(graceful.found);
            assert!(!graceful.stopped, "TERM-ignoring scope must stay active");
            assert!(!graceful.forced, "non-force close must not send SIGKILL");
            assert_eq!(graceful.detail, "scope_terminate_timeout");
            assert!(
                managed_agent_scope_control_group(&cutex_session_id)
                    .expect("query Agent scope after bounded graceful close")
                    .is_some(),
                "non-force close must preserve a TERM-ignoring Agent scope"
            );

            // The crashing PID is the scope leader recorded above. Linux
            // coredump attribution therefore names the Agent scope rather than
            // the launching service. The sleeping descendant deliberately
            // remains so lifecycle cleanup must target the whole cgroup.
            let signal_result = unsafe { libc::kill(root_pid as libc::pid_t, libc::SIGABRT) };
            assert_eq!(signal_result, 0, "send SIGABRT to scoped runtime");
            let status = child.wait().expect("reap crashing runtime");
            assert_eq!(status.signal(), Some(libc::SIGABRT));
            if systemd_coredump_is_configured() && systemd_journal_is_readable() {
                let (user_unit, crash_cgroup) =
                    wait_for_coredump_attribution(root_pid, &unit_name, Duration::from_secs(10))
                        .expect("systemd-coredump should journal the scoped crash");
                assert_eq!(user_unit, unit_name);
                assert!(crash_cgroup.ends_with(&format!("/{unit_name}")));
                assert!(!crash_cgroup.contains("cutex-agent-bus.service"));
            }
            assert!(
                managed_agent_scope_control_group(&cutex_session_id)
                    .expect("query post-crash Agent scope")
                    .is_some(),
                "descendant should keep the Agent scope populated"
            );

            let stopped = terminate_managed_agent_scope(&cutex_session_id, true)
                .expect("force-close Agent scope");
            assert!(stopped.found);
            assert!(stopped.stopped, "{}", stopped.detail);
            assert!(stopped.forced);
            assert_eq!(stopped.detail, "scope_force_killed");
            assert!(managed_agent_scope_control_group(&cutex_session_id)
                .expect("query collected Agent scope")
                .is_none());

            let restart_launch =
                LaunchCommand::new("sh").args(["-c", "printf 'restart_ready\\n'; exec sleep 30"]);
            restarted_child = Some(
                spawn_managed_agent_runtime_launch(
                    &cutex_session_id,
                    &restart_launch,
                    test_dir.to_str().expect("test path is UTF-8"),
                    &log_path,
                )
                .expect("restart runtime in collected Agent scope"),
            );
            wait_for_log(&log_path, "restart_ready", Duration::from_secs(5));
            assert!(
                managed_agent_scope_control_group(&cutex_session_id)
                    .expect("query restarted Agent scope")
                    .is_some_and(|group| group.ends_with(&format!("/{unit_name}"))),
                "restart must reuse the durable Agent boundary"
            );

            let stopped = terminate_managed_agent_scope(&cutex_session_id, false)
                .expect("gracefully close restarted Agent scope");
            assert!(stopped.found);
            assert!(stopped.stopped, "{}", stopped.detail);
            assert!(!stopped.forced);
            assert_eq!(stopped.detail, "scope_terminated");
            restarted_child
                .as_mut()
                .expect("restarted child was recorded")
                .wait()
                .expect("reap gracefully stopped restarted runtime");
            assert!(managed_agent_scope_control_group(&cutex_session_id)
                .expect("query Agent scope after graceful close")
                .is_none());
        }));

        let _ = terminate_managed_agent_scope(&cutex_session_id, true);
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(mut child) = restarted_child {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let _ = fs::remove_dir_all(&test_dir);
        if let Err(payload) = result {
            resume_unwind(payload);
        }
    }

    fn wait_for_log(path: &Path, marker: &str, timeout: Duration) -> String {
        let started = Instant::now();
        while started.elapsed() < timeout {
            let contents = fs::read_to_string(path).unwrap_or_default();
            if contents.contains(marker) {
                return contents;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "timed out waiting for {marker:?} in {}: {}",
            path.display(),
            fs::read_to_string(path).unwrap_or_default()
        );
    }

    fn log_field<'a>(log: &'a str, prefix: &str) -> &'a str {
        log.lines()
            .find_map(|line| line.strip_prefix(prefix))
            .map(str::trim)
            .unwrap_or_else(|| panic!("missing {prefix:?} in {log:?}"))
    }

    fn unified_cgroup(cgroups: &str) -> Option<&str> {
        cgroups
            .split_whitespace()
            .find_map(|entry| entry.strip_prefix("0::"))
    }

    fn cgroup_metric_path(control_group: &str, metric: &str) -> PathBuf {
        Path::new("/sys/fs/cgroup")
            .join(control_group.trim_start_matches('/'))
            .join(metric)
    }

    fn systemd_coredump_is_configured() -> bool {
        fs::read_to_string("/proc/sys/kernel/core_pattern")
            .ok()
            .is_some_and(|pattern| pattern.contains("systemd-coredump"))
    }

    fn systemd_journal_is_readable() -> bool {
        if !command_exists_in_path("journalctl") {
            return false;
        }
        Command::new("journalctl")
            .args(["--no-pager", "-n", "0", "-o", "json"])
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn wait_for_coredump_attribution(
        pid: u32,
        expected_user_unit: &str,
        timeout: Duration,
    ) -> Option<(String, String)> {
        let started = Instant::now();
        while started.elapsed() < timeout {
            let selector = format!("COREDUMP_PID={pid}");
            let output = Command::new("journalctl")
                .args(["--no-pager", "-o", "json", &selector])
                .stdin(Stdio::null())
                .output()
                .ok()?;
            if output.status.success() {
                for line in String::from_utf8_lossy(&output.stdout).lines().rev() {
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                        continue;
                    };
                    let Some(user_unit) = value
                        .get("COREDUMP_USER_UNIT")
                        .and_then(serde_json::Value::as_str)
                    else {
                        continue;
                    };
                    let Some(cgroup) = value
                        .get("COREDUMP_CGROUP")
                        .and_then(serde_json::Value::as_str)
                    else {
                        continue;
                    };
                    if user_unit != expected_user_unit {
                        continue;
                    }
                    return Some((user_unit.to_string(), cgroup.to_string()));
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        None
    }
}
