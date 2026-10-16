//! Windows managed installs: the `cutex service run` supervisor (started by
//! the `\Cutex\cutex-supervisor` task `cutex service install` registers)
//! owns the Agent Bus and the Management API as one unit. Start brings
//! both up (starting the task first when no supervisor runs); Stop and
//! Restart act on both, Management first.
use super::{Action, Service};
use cutex::upgrade::layout::Layout;
use cutex::upgrade::service::{ServiceControl, ServiceNames, SupervisorControl};
use cutex::upgrade::supervisor::{self, ChildStatus};
use std::time::Duration;

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(super) fn run(layout: &Layout, service: Service, action: Action, timeout: Duration) -> anyhow::Result<String> {
    let record = layout.read_install()?;
    let control = SupervisorControl { layout, timeout, task_name: ServiceNames::of(&record).task_name };
    match action {
        Action::Status => {}
        Action::Start => control.start_all()?,
        Action::Stop => {
            if supervisor::running(layout) {
                control.stop_all()?;
            }
        }
        Action::Restart => {
            if supervisor::running(layout) {
                control.stop_all()?;
            }
            control.start_all()?;
        }
    }
    summarize(layout, service, action)
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn summarize(layout: &Layout, service: Service, action: Action) -> anyhow::Result<String> {
    let running = supervisor::running(layout);
    let status = supervisor::read_status(layout);
    let child = status.as_ref().map(|status| match service {
        Service::Bus => &status.bus,
        Service::Management => &status.management,
    });
    let state = match (running, child) {
        (true, Some(child)) => child.state.clone(),
        // A supervisor that died leaves its children running (the next one
        // adopts them); without one nothing restarts them.
        (false, Some(ChildStatus { pid: Some(_), .. })) => "unsupervised".into(),
        _ => "stopped".into(),
    };
    let mut text = format!(
        "{} · cutex supervisor ({})\nState: {state} · Supervisor: {}",
        service.name(),
        layout.base.display(),
        if running { "running" } else { "not running" }
    );
    if let Some(child) = child.filter(|_| running) {
        if let Some(pid) = child.pid {
            text.push_str(&format!("\nPID: {pid}"));
        }
        if child.restarts > 0 {
            text.push_str(&format!("\nRestarts: {}", child.restarts));
        }
        if let Some(exit) = &child.last_exit {
            text.push_str(&format!("\nLast exit: {exit}"));
        }
    }
    if matches!(action, Action::Stop | Action::Restart) {
        text.push_str("\nThe supervisor runs Agent Bus and Management API together: both were stopped");
        if action == Action::Restart {
            text.push_str(" and started again");
        }
        text.push('.');
    }
    if let Some(dir) = supervisor::log_dir_hint() {
        text.push_str(&format!("\nLogs: {}", dir.display()));
    }
    Ok(text)
}

/// The managed install the CLI belongs to: `CUTEX_INSTALL_BASE` / the
/// running executable's layout, else the platform default base when it holds
/// an `install.json`.
pub(super) fn managed_layout() -> Option<Layout> {
    Layout::detect().or_else(|| Layout::default_layout().filter(Layout::is_managed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutex::upgrade::supervisor::{Control, Desired, Status, STATUS_SCHEMA};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn layout() -> Layout {
        let dir = std::env::temp_dir().join(format!("cutex-svc-supervisor-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("run")).unwrap();
        std::fs::write(
            dir.join("install.json"),
            serde_json::json!({
                "schema": "cutex/install/v1", "os": "windows", "serviceManager": "cutex-supervisor",
                "channel": "release", "createdAt": "2026-10-16T00:00:00Z", "activeRelease": "r1",
                "taskName": "\\Cutex-test\\cutex-supervisor"
            })
            .to_string(),
        )
        .unwrap();
        Layout::at(dir)
    }

    fn child(state: &str, pid: Option<u32>) -> ChildStatus {
        ChildStatus { state: state.into(), pid, ..Default::default() }
    }

    fn write_status(layout: &Layout, desired: Desired, ack_seq: u64) {
        let (bus, management) = if desired == Desired::Running {
            (child("running", Some(11)), child("running", Some(12)))
        } else {
            (child("stopped", None), child("stopped", None))
        };
        let status = Status {
            schema: STATUS_SCHEMA.into(),
            pid: std::process::id(),
            version: "test".into(),
            started_at: "t".into(),
            heartbeat_at: "t".into(),
            desired,
            ack_seq,
            bus,
            management,
        };
        std::fs::write(supervisor::status_path(layout), serde_json::to_vec(&status).unwrap()).unwrap();
    }

    /// A stand-in supervisor: holds the lock and acknowledges control requests.
    fn fake_supervisor(layout: &Layout, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<Vec<Desired>> {
        let layout = layout.clone();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(supervisor::lock_path(&layout))
            .unwrap();
        fs2::FileExt::lock_exclusive(&lock).unwrap();
        write_status(&layout, Desired::Running, 0);
        std::thread::spawn(move || {
            let _lock = lock;
            let mut seen = Vec::new();
            let mut acked = 0;
            while !stop.load(Ordering::SeqCst) {
                if let Some(Control { seq, desired, .. }) = supervisor::read_control(&layout) {
                    if seq > acked {
                        seen.push(desired);
                        acked = seq;
                        write_status(&layout, desired, seq);
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            seen
        })
    }

    #[test]
    fn status_without_supervisor_reports_stopped_or_unsupervised() {
        let layout = layout();
        let text = run(&layout, Service::Bus, Action::Status, Duration::from_secs(5)).unwrap();
        assert!(text.contains("State: stopped") && text.contains("Supervisor: not running"), "{text}");
        // Stop with no supervisor: nothing to do, no schtasks call.
        assert!(run(&layout, Service::Management, Action::Stop, Duration::from_secs(5)).unwrap().contains("both were stopped"));
        write_status(&layout, Desired::Running, 0);
        let text = run(&layout, Service::Management, Action::Status, Duration::from_secs(5)).unwrap();
        assert!(text.contains("State: unsupervised"), "{text}");
        std::fs::remove_dir_all(&layout.base).unwrap();
    }

    #[test]
    fn start_stop_restart_go_through_the_supervisor_control_file() {
        let layout = layout();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = fake_supervisor(&layout, stop.clone());
        let text = run(&layout, Service::Management, Action::Status, Duration::from_secs(5)).unwrap();
        assert!(text.contains("State: running") && text.contains("PID: 12"), "{text}");
        let text = run(&layout, Service::Bus, Action::Stop, Duration::from_secs(5)).unwrap();
        assert!(text.contains("State: stopped") && text.contains("both were stopped"), "{text}");
        let text = run(&layout, Service::Bus, Action::Start, Duration::from_secs(5)).unwrap();
        assert!(text.contains("State: running") && text.contains("PID: 11"), "{text}");
        let text = run(&layout, Service::Management, Action::Restart, Duration::from_secs(5)).unwrap();
        assert!(text.contains("State: running") && text.contains("started again"), "{text}");
        stop.store(true, Ordering::SeqCst);
        let seen = handle.join().unwrap();
        assert_eq!(seen, vec![Desired::Stopped, Desired::Running, Desired::Stopped, Desired::Running]);
        std::fs::remove_dir_all(&layout.base).unwrap();
    }
}
