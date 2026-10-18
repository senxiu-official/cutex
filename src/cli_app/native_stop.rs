//! Stop the proven native occurrence, independently of next-launch settings.
use anyhow::{Context, ensure};
use cutex::platform::process::ProcessTerminateOutcome;
use cutex::session::model::CutexSessionRecord;

pub(super) fn stop_and_commit(
    record: &CutexSessionRecord,
    force: bool,
) -> anyhow::Result<ProcessTerminateOutcome> {
    let outcome = stop_processes(record, force)?;
    if outcome.stopped {
        let _ = super::app_server_runtime::disconnect_runtime(&record.cutex_session_id);
        cutex::agent_management::commit_stock_runtime_stop(
            &cutex::session::store::cutex_sessions_path()?,
            record,
        )?;
    }
    Ok(outcome)
}

/// No SessionStore writes or manager joins: also usable inside an archive transaction.
pub(super) fn stop_processes(
    record: &CutexSessionRecord,
    force: bool,
) -> anyhow::Result<ProcessTerminateOutcome> {
    ensure!(
        cutex::runtime::lifecycle::cutex_session_host_is_local(
            &record.host_id,
            &cutex::platform::host::current_host_name()
        ),
        "native owner belongs to another host"
    );
    let Some(binding) = &record.app_server_runtime else {
        ensure!(
            !cutex::session::archive::record_has_runtime_claim(record),
            "native launch unresolved; recover its original action"
        );
        return Ok(ProcessTerminateOutcome {
            stopped: true,
            forced: false,
            detail: "already_offline".into(),
        });
    };
    // An occurrence recorded before this boot cannot be running. Its PIDs and
    // a captured stop scope (pid + birth ticks restart at boot) may now match
    // unrelated processes: never signal anything for it.
    if cutex::session::boot_fence::with_host_probe(|host| {
        cutex::session::boot_fence::binding_process(binding, host)
            == cutex::session::boot_fence::BindingProcess::PreviousBoot
    }) {
        return Ok(ProcessTerminateOutcome {
            stopped: true,
            forced: false,
            detail: "native_owner_previous_boot".into(),
        });
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        stop_linux(record, binding, force)
    }
    #[cfg(windows)]
    {
        // Reconciliation of an already-dead owner must not depend on its retired
        // historical package.  A live or reused PID still takes the strict
        // receipt/bundle/start-time/image path below.
        if !cutex::platform::process::process_is_running(binding.pid) {
            return Ok(ProcessTerminateOutcome {
                stopped: true,
                forced: false,
                detail: "owned_runtime_process_absent".into(),
            });
        }
        let sessions = cutex::session::store::load_cutex_session_store()?;
        let receipt = sessions.explicit_launch_receipts.values().find_map(|receipt| {
            if let cutex::agent_management::ExplicitLaunchActionReceipt::Runtime(receipt) = receipt {
                (receipt.review.subject.cutex_session_id.as_str() == record.cutex_session_id
                    && receipt.binding.as_ref() == Some(binding)).then_some(receipt)
            } else { None }
        }).context("runtime publication receipt missing")?;
        let bundle = cutex::launch::stock::StockBundle::load_running(&receipt.review.contract)?;
        let process = super::stock_publication::verified_process(binding.pid, &binding.started_at,
            &bundle.executable.path)?;
        super::stock_publication::stop_published_job(
            receipt.publication.as_ref().context("runtime publication evidence missing")?, process.as_ref())?;
        Ok(ProcessTerminateOutcome { stopped: true, forced: force, detail: "owned_runtime_job_stopped".into() })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (binding, force);
        anyhow::bail!("native process stop requires supported process identity")
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Member {
    pid: u32,
    parent: u32,
    group: u32,
    session: u32,
    ticks: u64,
    state: String,
}
#[cfg(target_os = "linux")]
fn member(pid: u32) -> anyhow::Result<Option<Member>> {
    let raw = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let parts = raw[raw.rfind(')').context("invalid proc stat")? + 1..]
        .split_whitespace()
        .collect::<Vec<_>>();
    ensure!(parts.len() > 19, "short proc stat");
    Ok(Some(Member {
        pid,
        parent: parts[1].parse()?,
        group: parts[2].parse()?,
        session: parts[3].parse()?,
        ticks: parts[19].parse()?,
        state: parts[0].into(),
    }))
}
/// macOS: libproc identity (docs/macos/PROBE.md). `ticks` is the start time
/// in microseconds: fixed for a process's life, different after PID reuse.
#[cfg(target_os = "macos")]
fn member_of(stat: cutex::platform::macos_proc::ProcStat) -> Member {
    Member {
        pid: stat.pid,
        parent: stat.ppid,
        group: stat.pgid,
        session: stat.sid,
        ticks: stat.start_ticks,
        state: if stat.zombie { "Z" } else { "S" }.into(),
    }
}
#[cfg(target_os = "macos")]
fn member(pid: u32) -> anyhow::Result<Option<Member>> {
    Ok(cutex::platform::macos_proc::stat(pid)?.map(member_of))
}
#[cfg(target_os = "macos")]
fn process_snapshot() -> anyhow::Result<Vec<Member>> {
    Ok(cutex::platform::macos_proc::snapshot()?.into_iter().map(member_of).collect())
}
#[cfg(target_os = "linux")]
fn process_snapshot() -> anyhow::Result<Vec<Member>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if let Some(m) = member(pid)? {
            found.push(m);
        }
    }
    Ok(found)
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn members(group: u32) -> anyhow::Result<Vec<Member>> {
    Ok(process_snapshot()?
        .into_iter()
        .filter(|m| {
            (m.group == group || m.session == group) && !matches!(m.state.as_str(), "Z" | "X")
        })
        .collect())
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(serde::Serialize, serde::Deserialize)]
struct StopScope {
    version: u32,
    session_id: String,
    native_id: Option<String>,
    generation: u64,
    binding: cutex::session::model::CutexAppServerRuntimeBinding,
    members: Vec<Member>,
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn scope_path(binding: &cutex::session::model::CutexAppServerRuntimeBinding) -> std::path::PathBuf {
    std::path::Path::new(&binding.runtime_dir).join("stop-scope.json")
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn load_scope(
    record: &CutexSessionRecord,
    binding: &cutex::session::model::CutexAppServerRuntimeBinding,
) -> anyhow::Result<Option<StopScope>> {
    let data = match std::fs::read(scope_path(binding)) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let scope: StopScope = serde_json::from_slice(&data)?;
    ensure!(
        scope.version == 1
            && scope.session_id == record.cutex_session_id
            && scope.native_id == record.codex_session_id
            && scope.generation == record.runtime_generation
            && scope.binding == *binding,
        "native stop scope occurrence mismatch"
    );
    Ok(Some(scope))
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn live_scope(scope: &mut StopScope, snapshot: &[Member]) -> Vec<Member> {
    // Birth ticks, not current PID alone, carry ownership across reparenting and
    // setsid(). Only a currently proven member can authorize session expansion.
    let mut live = snapshot
        .iter()
        .filter(|m| {
            scope
                .members
                .iter()
                .any(|old| old.pid == m.pid && old.ticks == m.ticks)
        })
        .cloned()
        .collect::<Vec<_>>();
    loop {
        let added = snapshot
            .iter()
            .filter(|m| {
                !live.iter().any(|v| v.pid == m.pid)
                    && live.iter().any(|owner| {
                        m.parent == owner.pid
                            || m.session == owner.session
                            || m.group == owner.group
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        if added.is_empty() {
            break;
        }
        live.extend(added);
    }
    for m in &live {
        if !scope
            .members
            .iter()
            .any(|old| old.pid == m.pid && old.ticks == m.ticks)
        {
            scope.members.push(m.clone());
        }
    }
    live.into_iter()
        .filter(|m| !matches!(m.state.as_str(), "Z" | "X"))
        .collect()
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn scope_lock(
    binding: &cutex::session::model::CutexAppServerRuntimeBinding,
) -> anyhow::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let dir = std::path::Path::new(&binding.runtime_dir);
    let meta = std::fs::symlink_metadata(dir)?;
    ensure!(
        dir.is_absolute()
            && meta.is_dir()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0,
        "native stop scope directory must be private and owned"
    );
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(dir.join("stop-scope.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    Ok(lock)
}
/// macOS has no pidfd: re-read the identity, then signal the PID. The window
/// between the check and kill(2) is the PID-reuse race pidfd closes on Linux;
/// macOS assigns PIDs sequentially up to 99999, so reuse inside that window
/// needs the whole PID space to wrap in microseconds.
#[cfg(target_os = "macos")]
fn signal_exact(m: &Member, signal: i32) -> anyhow::Result<()> {
    let Some(now) = member(m.pid)? else {
        return Ok(());
    };
    if now.ticks != m.ticks || now.group != m.group || now.session != m.session {
        return Ok(());
    }
    if unsafe { libc::kill(m.pid as libc::pid_t, signal) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            return Err(e.into());
        }
    }
    Ok(())
}
#[cfg(target_os = "linux")]
fn signal_exact(m: &Member, signal: i32) -> anyhow::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, m.pid, 0) as i32 };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(e.into());
    }
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let Some(now) = member(m.pid)? else {
        return Ok(());
    };
    if now.ticks != m.ticks || now.group != m.group || now.session != m.session {
        return Ok(());
    }
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            return Err(e.into());
        }
    }
    Ok(())
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn stop_linux(
    record: &CutexSessionRecord,
    binding: &cutex::session::model::CutexAppServerRuntimeBinding,
    force: bool,
) -> anyhow::Result<ProcessTerminateOutcome> {
    let pid = binding.pid;
    ensure!(
        pid > 1 && pid <= i32::MAX as u32,
        "invalid native owner PID"
    );
    let mut _lock = if std::path::Path::new(&binding.runtime_dir).exists() {
        Some(scope_lock(binding)?)
    } else {
        None
    };
    let mut scope = load_scope(record, binding)?;
    let expected = chrono::DateTime::parse_from_rfc3339(&binding.started_at)?;
    if scope.is_none() {
        if let Some(observed) = member(pid)? {
            let actual = cutex::platform::process::process_started_at(pid)?;
            if actual.timestamp_millis() != expected.timestamp_millis() {
                return Ok(ProcessTerminateOutcome {
                    stopped: true,
                    forced: false,
                    detail: "stale_native_pid_reused_preserved".into(),
                });
            }
            if !matches!(observed.state.as_str(), "Z" | "X") {
                super::stock_lifecycle::verify_stock_process(record, binding)?;
            }
            let owner = member(pid)?.context("native owner exited during verification")?;
            ensure!(
                owner.ticks == observed.ticks && owner.group == pid && owner.session == pid,
                "native owner birth or dedicated process session changed"
            );
            scope = Some(StopScope {
                version: 1,
                session_id: record.cutex_session_id.clone(),
                native_id: record.codex_session_id.clone(),
                generation: record.runtime_generation,
                binding: binding.clone(),
                members: vec![owner],
            });
        } else {
            // A missing leader and an old numeric SID cannot prove that a
            // remaining session belongs to this occurrence after PID reuse.
            ensure!(
                members(pid)?.is_empty(),
                "native leader absent without captured stop scope; remaining session ownership unproven"
            );
            return Ok(ProcessTerminateOutcome {
                stopped: true,
                forced: false,
                detail: "native_owner_absent".into(),
            });
        }
    }
    if _lock.is_none() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&binding.runtime_dir)?;
        _lock = Some(scope_lock(binding)?);
    }
    stop_captured_scope(scope.as_mut().unwrap(), force)
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn refresh_and_persist_scope(
    scope: &mut StopScope,
    persisted: &mut Option<usize>,
) -> anyhow::Result<Vec<Member>> {
    let live = live_scope(scope, &process_snapshot()?);
    if *persisted != Some(scope.members.len()) {
        cutex::config::atomic::write_private_pretty_json_atomic(
            &scope_path(&scope.binding),
            scope,
            "native stop scope",
        )?;
        *persisted = Some(scope.members.len());
    }
    Ok(live)
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn stop_captured_scope(
    scope: &mut StopScope,
    force: bool,
) -> anyhow::Result<ProcessTerminateOutcome> {
    let mut forced = false;
    let mut persisted = None;
    for signal in [libc::SIGTERM, libc::SIGKILL] {
        if signal == libc::SIGKILL {
            if !force {
                break;
            }
            forced = true;
        }
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            // Persist the captured family before signaling; retries retain
            // detached descendants after their parent exits.
            let live = refresh_and_persist_scope(scope, &mut persisted)?;
            if live.is_empty() {
                return Ok(ProcessTerminateOutcome {
                    stopped: true,
                    forced,
                    detail: "native_captured_processes_stopped".into(),
                });
            }
            for m in &live {
                signal_exact(m, signal)?;
            }
            if std::time::Instant::now() >= until {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    Ok(ProcessTerminateOutcome {
        stopped: refresh_and_persist_scope(scope, &mut persisted)?.is_empty(),
        forced,
        detail: "native_captured_processes_still_running".into(),
    })
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-(self.0.id() as i32), libc::SIGKILL);
            }
            let _ = self.0.wait();
        }
    }
    fn child(code: &str) -> Child {
        let mut cmd = Command::new("python3");
        cmd.args(["-c", code])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = Child(cmd.spawn().unwrap());
        use std::io::BufRead;
        let mut ready = String::new();
        std::io::BufReader::new(child.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready.trim(), "ready");
        child
    }
    fn scope(c: &Child, dir: &std::path::Path) -> (CutexSessionRecord, StopScope) {
        let mut record = CutexSessionRecord::new(
            "cutex.test".into(),
            Some(uuid::Uuid::new_v4().to_string()),
            cutex::platform::host::current_host_name(),
            "/tmp".into(),
            None,
        )
        .unwrap();
        let binding = serde_json::from_value(serde_json::json!({"transport":"unix_socket","endpoint":"unix:///missing","pid":c.0.id(),"runtime_dir":dir,"diagnostic_journal_path":"/missing/log","schema_version":"test","schema_sha256":"0".repeat(64),"started_at":cutex::platform::process::process_started_at(c.0.id()).unwrap().to_rfc3339()})).unwrap();
        record.app_server_runtime = Some(binding);
        let scope = StopScope {
            version: 1,
            session_id: record.cutex_session_id.clone(),
            native_id: record.codex_session_id.clone(),
            generation: record.runtime_generation,
            binding: record.app_server_runtime.clone().unwrap(),
            members: vec![member(c.0.id()).unwrap().unwrap()],
        };
        (record, scope)
    }
    struct Family(Vec<Member>);
    impl Drop for Family {
        fn drop(&mut self) {
            for m in &self.0 {
                let _ = signal_exact(m, libc::SIGKILL);
            }
        }
    }
    #[test]
    fn native_stop_captures_detached_children_and_retries_after_parent_exits() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new("stop-family").unwrap();
        let c = child(
            "import os,time,signal\nr,w=os.pipe()\npid=os.fork()\nif pid==0:\n os.setsid()\n signal.signal(signal.SIGTERM,signal.SIG_IGN)\n kid=os.fork()\n if kid==0:\n  os.setsid()\n  os.write(w,b'1')\n while True: time.sleep(1)\nelse:\n os.read(r,1)\n print('ready',flush=True)\n while True: time.sleep(1)",
        );
        let (record, mut captured) = scope(&c, home.root());
        let guard = Family(live_scope(&mut captured, &process_snapshot().unwrap()));
        assert_eq!(guard.0.len(), 3);
        let first = stop_captured_scope(&mut captured, false).unwrap();
        assert!(!first.stopped);
        assert!(verify_stopped(&record).is_err());
        let mut restored = load_scope(&record, &captured.binding).unwrap().unwrap();
        assert_eq!(restored.members.len(), 3);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(scope_path(&captured.binding))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let final_result = stop_linux(&record, &captured.binding, true).unwrap();
        assert!(final_result.stopped && final_result.forced);
        verify_stopped(&record).unwrap();
        assert!(live_scope(&mut restored, &process_snapshot().unwrap()).is_empty());
    }
    #[test]
    fn missing_legacy_leader_does_not_authorize_numeric_session_signals() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new("stop-no-proof").unwrap();
        let mut c = child(
            "import os,time,signal\nr,w=os.pipe()\npid=os.fork()\nif pid==0:\n signal.signal(signal.SIGTERM,signal.SIG_IGN)\n os.write(w,b'1')\n while True: time.sleep(1)\nelse:\n os.read(r,1)\n print('ready',flush=True)\n while True: time.sleep(1)",
        );
        let (record, mut captured) = scope(&c, home.root());
        let guard = Family(live_scope(&mut captured, &process_snapshot().unwrap()));
        assert_eq!(guard.0.len(), 2);
        c.0.kill().unwrap();
        c.0.wait().unwrap();
        assert!(stop_linux(&record, &captured.binding, true).is_err());
        assert_eq!(members(c.0.id()).unwrap().len(), 1);
    }
    #[test]
    fn missing_legacy_leader_without_remaining_scope_can_clear() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new("stop-empty-old").unwrap();
        let mut c = child("import time\nprint('ready',flush=True)\ntime.sleep(60)");
        let (record, captured) = scope(&c, home.root());
        c.0.kill().unwrap();
        c.0.wait().unwrap();
        assert!(
            stop_linux(&record, &captured.binding, false)
                .unwrap()
                .stopped
        );
        verify_stopped(&record).unwrap();
        assert!(!scope_path(&captured.binding).exists());
    }
    #[test]
    fn captured_reused_pid_is_not_signaled_or_used_to_expand_scope() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new("stop-reused").unwrap();
        let mut c = child("import time\nprint('ready',flush=True)\ntime.sleep(60)");
        let (record, mut captured) = scope(&c, home.root());
        captured.members[0].ticks -= 1;
        assert!(stop_captured_scope(&mut captured, true).unwrap().stopped);
        assert!(c.0.try_wait().unwrap().is_none());
        verify_stopped(&record).unwrap();
        let mut wrong = record.clone();
        wrong.runtime_generation += 1;
        assert!(load_scope(&wrong, &captured.binding).is_err());
    }
    #[test]
    fn stale_native_binding_does_not_signal_reused_pid() {
        let c = child("import time\nprint('ready',flush=True)\ntime.sleep(60)");
        let mut record = CutexSessionRecord::new(
            "cutex.test".into(),
            Some(uuid::Uuid::new_v4().to_string()),
            cutex::platform::host::current_host_name(),
            "/tmp".into(),
            None,
        )
        .unwrap();
        let binding:cutex::session::model::CutexAppServerRuntimeBinding=serde_json::from_value(serde_json::json!({"transport":"unix_socket","endpoint":"unix:///missing","pid":c.0.id(),"runtime_dir":"/missing","diagnostic_journal_path":"/missing/log","schema_version":"test","schema_sha256":"0".repeat(64),"started_at":"2000-01-01T00:00:00Z"})).unwrap();
        record.app_server_runtime = Some(binding.clone());
        let result = stop_linux(&record, &binding, true).unwrap();
        assert!(result.stopped);
        assert_eq!(result.detail, "stale_native_pid_reused_preserved");
        assert!(cutex::platform::process::process_is_running(c.0.id()));
    }
    /// After a reboot, PIDs and birth ticks restart: a stop scope captured in
    /// the previous boot can match unrelated live processes. An occurrence
    /// recorded before this boot is never signaled.
    #[test]
    fn previous_boot_binding_never_signals_a_matching_scope() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new("stop-old-boot").unwrap();
        let c = child("import time\nprint('ready',flush=True)\ntime.sleep(60)");
        let (mut record, mut captured) = scope(&c, home.root());
        let boot = cutex::platform::process::system_boot_time().unwrap();
        let mut binding = record.app_server_runtime.clone().unwrap();
        binding.started_at = (boot - chrono::Duration::hours(1)).to_rfc3339();
        record.app_server_runtime = Some(binding.clone());
        captured.binding = binding;
        let mut persisted = None;
        let _lock = scope_lock(&captured.binding).unwrap();
        assert_eq!(refresh_and_persist_scope(&mut captured, &mut persisted).unwrap().len(), 1);
        drop(_lock);
        let result = stop_processes(&record, true).unwrap();
        assert!(result.stopped && !result.forced);
        assert_eq!(result.detail, "native_owner_previous_boot");
        assert!(cutex::platform::process::process_is_running(c.0.id()));
    }
}

/// Kernel observation after stop; never emits a signal.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn verify_stopped(record: &CutexSessionRecord) -> anyhow::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Some(binding) = &record.app_server_runtime {
        let _lock = if scope_path(binding).exists() {
            Some(scope_lock(binding)?)
        } else {
            None
        };
        if let Some(mut scope) = load_scope(record, binding)? {
            ensure!(
                live_scope(&mut scope, &process_snapshot()?).is_empty(),
                "captured native descendants still running"
            );
            return Ok(());
        }
        if let Some(m) = member(binding.pid)? {
            let expected = chrono::DateTime::parse_from_rfc3339(&binding.started_at)?;
            let actual = cutex::platform::process::process_started_at(binding.pid)?;
            if actual.timestamp_millis() != expected.timestamp_millis() {
                return Ok(());
            }
            ensure!(
                matches!(m.state.as_str(), "Z" | "X"),
                "native owner still running"
            );
        }
        ensure!(
            members(binding.pid)?.is_empty(),
            "native process session still running"
        );
    }
    Ok(())
}
