//! `cutex service run`: a small per-user supervisor that keeps the Agent Bus
//! and the Management API running (Windows colleague installs; RFC §4.4
//! without runtime-host). Portable Rust, so the logic is tested on Linux.
//!
//! - Started at logon by the Task Scheduler task `cutex service install`
//!   registers (or by the desktop when Management does not answer); one
//!   instance per install (`run/supervisor.lock`).
//! - Starts `cutex agent serve`, waits for the bus to answer, then
//!   `cutex management serve`, both from the layout's service executable
//!   (`bin\cutex.exe`, swapped by rename during an upgrade), with
//!   `CUTEX_SERVICE_MANAGER=cutex-supervisor` and `CUTEX_INSTALL_BASE`.
//! - Restarts a child that exits, with backoff 1 s, 2 s, 4 s … 60 s (reset
//!   after 10 minutes up).
//! - Stops by terminating exactly the child process (never by image name,
//!   never the process tree): native app-servers started by Management live
//!   on, which is what an upgrade in reload mode relies on.
//! - Status: `run/supervisor.json` (heartbeat every 2 s). Control:
//!   `run/supervisor-control.json` `{seq, desired: running|stopped|exit}`;
//!   the status echoes `ackSeq` once the desired state is reached.
//! - If the supervisor itself dies, its children keep running; the next
//!   supervisor adopts them by the PIDs (and creation times) it recorded.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

use super::layout::Layout;

pub const STATUS_SCHEMA: &str = "cutex/supervisor-status/v1";
const TICK: Duration = Duration::from_millis(250);
const HEARTBEAT: Duration = Duration::from_secs(2);
pub const STALE_AFTER: Duration = Duration::from_secs(15);
const BUS_READY_WAIT: Duration = Duration::from_secs(30);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const BACKOFF_RESET: Duration = Duration::from_secs(600);
const STOP_GRACE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Desired {
    Running,
    Stopped,
    Exit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Control {
    pub seq: u64,
    pub desired: Desired,
    #[serde(default)]
    pub requested_at: Option<String>,
    #[serde(default)]
    pub by: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildStatus {
    /// `running` | `starting` | `stopped` | `backoff` | `exited`.
    pub state: String,
    #[serde(default)]
    pub pid: Option<u32>,
    /// Process creation time (RFC 3339) for PID-reuse checks on adoption.
    #[serde(default)]
    pub process_started_at: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub restarts: u32,
    #[serde(default)]
    pub last_exit: Option<String>,
    #[serde(default)]
    pub adopted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub schema: String,
    pub pid: u32,
    pub version: String,
    pub started_at: String,
    pub heartbeat_at: String,
    pub desired: Desired,
    pub ack_seq: u64,
    pub bus: ChildStatus,
    pub management: ChildStatus,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn status_path(layout: &Layout) -> PathBuf {
    layout.supervisor_dir().join("supervisor.json")
}
pub fn control_path(layout: &Layout) -> PathBuf {
    layout.supervisor_dir().join("supervisor-control.json")
}
pub fn lock_path(layout: &Layout) -> PathBuf {
    layout.supervisor_dir().join("supervisor.lock")
}

pub fn read_status(layout: &Layout) -> Option<Status> {
    serde_json::from_slice(&std::fs::read(status_path(layout)).ok()?).ok()
}

pub fn read_control(layout: &Layout) -> Option<Control> {
    serde_json::from_slice(&std::fs::read(control_path(layout)).ok()?).ok()
}

/// A supervisor holds the lock (it is running).
pub fn running(layout: &Layout) -> bool {
    let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).create(false).open(lock_path(layout)) else {
        return false;
    };
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&file);
            false
        }
        Err(_) => true,
    }
}

/// Ask the supervisor for a desired state; returns the request's seq.
pub fn request(layout: &Layout, desired: Desired, by: &str) -> anyhow::Result<u64> {
    std::fs::create_dir_all(layout.supervisor_dir())?;
    let seq = read_control(layout).map_or(0, |c| c.seq).max(read_status(layout).map_or(0, |s| s.ack_seq)) + 1;
    let control = Control { seq, desired, requested_at: Some(now()), by: Some(by.into()) };
    crate::config::atomic::write_private_pretty_json_atomic(&control_path(layout), &control, "supervisor control")?;
    Ok(seq)
}

/// Wait until the supervisor acknowledged `seq` (desired state reached).
pub fn wait_ack(layout: &Layout, seq: u64, timeout: Duration) -> anyhow::Result<Status> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = read_status(layout) {
            if status.ack_seq >= seq {
                return Ok(status);
            }
        }
        if Instant::now() > deadline {
            bail!("the cutex supervisor did not confirm the request within {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ---------------------------------------------------------------- children

pub trait Child: Send {
    fn id(&self) -> u32;
    /// `Some(description)` once exited.
    fn poll_exit(&mut self) -> Option<String>;
    /// Stop this exact process (graceful first where the OS allows it).
    fn stop(&mut self, grace: Duration);
}

pub trait Spawner {
    fn spawn(&self, service: Service) -> anyhow::Result<Box<dyn Child>>;
    /// The service answers (bus health; management health).
    fn healthy(&self, service: Service) -> bool;
    /// Adopt a running process recorded by an earlier supervisor.
    fn adopt(&self, service: Service, pid: u32, process_started_at: Option<&str>) -> Option<Box<dyn Child>>;
    fn process_started_at(&self, pid: u32) -> Option<String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Bus,
    Management,
}

impl Service {
    pub fn args(self) -> &'static [&'static str] {
        match self {
            Self::Bus => &["agent", "serve"],
            Self::Management => &["management", "serve"],
        }
    }
    fn log_name(self) -> &'static str {
        match self {
            Self::Bus => "agent-bus.log",
            Self::Management => "management.log",
        }
    }
}

struct Slot {
    service: Service,
    child: Option<Box<dyn Child>>,
    status: ChildStatus,
    since: Option<Instant>,
    backoff_until: Option<Instant>,
    failures: u32,
}

impl Slot {
    fn new(service: Service) -> Self {
        Self {
            service,
            child: None,
            status: ChildStatus { state: "stopped".into(), ..Default::default() },
            since: None,
            backoff_until: None,
            failures: 0,
        }
    }

    fn reap(&mut self, now: Instant) {
        let Some(child) = self.child.as_mut() else { return };
        let Some(exit) = child.poll_exit() else { return };
        let ran = self.since.map_or(Duration::ZERO, |since| now - since);
        if ran >= BACKOFF_RESET {
            self.failures = 0;
        }
        self.failures += 1;
        let backoff = Duration::from_secs(1u64 << self.failures.min(6).saturating_sub(1)).min(BACKOFF_MAX);
        self.backoff_until = Some(now + backoff);
        self.child = None;
        self.since = None;
        self.status.state = "backoff".into();
        self.status.pid = None;
        self.status.process_started_at = None;
        self.status.adopted = false;
        self.status.last_exit = Some(format!("{exit} at {}", super::supervisor::now()));
    }

    fn start(&mut self, spawner: &dyn Spawner, now: Instant) {
        if self.child.is_some() || self.backoff_until.is_some_and(|until| now < until) {
            return;
        }
        match spawner.spawn(self.service) {
            Ok(child) => {
                let pid = child.id();
                if self.status.started_at.is_some() {
                    self.status.restarts += 1;
                }
                self.status = ChildStatus {
                    state: "running".into(),
                    pid: Some(pid),
                    process_started_at: spawner.process_started_at(pid),
                    started_at: Some(super::supervisor::now()),
                    restarts: self.status.restarts,
                    last_exit: self.status.last_exit.clone(),
                    adopted: false,
                };
                self.child = Some(child);
                self.since = Some(now);
                self.backoff_until = None;
            }
            Err(error) => {
                self.failures += 1;
                self.backoff_until = Some(now + Duration::from_secs(1u64 << self.failures.min(6).saturating_sub(1)).min(BACKOFF_MAX));
                self.status.state = "backoff".into();
                self.status.last_exit = Some(format!("spawn failed: {error:#}"));
            }
        }
    }

    fn stop(&mut self, grace: Duration) {
        if let Some(mut child) = self.child.take() {
            child.stop(grace);
        }
        self.status.state = "stopped".into();
        self.status.pid = None;
        self.status.process_started_at = None;
        self.status.adopted = false;
        self.since = None;
        self.backoff_until = None;
        self.failures = 0;
    }

    fn running(&self) -> bool {
        self.child.is_some()
    }
}

/// The supervisor loop. Returns when asked to exit (`Desired::Exit`).
pub fn run_loop(layout: &Layout, spawner: &dyn Spawner, max_ticks: Option<u64>) -> anyhow::Result<()> {
    std::fs::create_dir_all(layout.supervisor_dir())?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path(layout))?;
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        bail!("a cutex supervisor already runs for {}", layout.base.display());
    }
    let started_at = now();
    let mut bus = Slot::new(Service::Bus);
    let mut management = Slot::new(Service::Management);
    // Adopt children an earlier supervisor left running.
    if let Some(previous) = read_status(layout) {
        for (slot, recorded) in [(&mut bus, &previous.bus), (&mut management, &previous.management)] {
            if let Some(pid) = recorded.pid {
                if let Some(child) = spawner.adopt(slot.service, pid, recorded.process_started_at.as_deref()) {
                    slot.child = Some(child);
                    slot.since = Some(Instant::now());
                    slot.status = ChildStatus { state: "running".into(), adopted: true, ..recorded.clone() };
                }
            }
        }
    }
    let mut desired = Desired::Running;
    let mut ack_seq = read_status(layout).map_or(0, |s| s.ack_seq);
    let mut pending_seq: Option<u64> = None;
    let mut bus_started: Option<Instant> = None;
    let mut last_written: Option<(Instant, Status)> = None;
    let mut ticks: u64 = 0;
    loop {
        let now_instant = Instant::now();
        if let Some(control) = read_control(layout) {
            if control.seq > ack_seq && pending_seq != Some(control.seq) {
                desired = control.desired;
                pending_seq = Some(control.seq);
            }
        }
        bus.reap(now_instant);
        management.reap(now_instant);
        match desired {
            Desired::Running => {
                if !bus.running() {
                    bus.start(spawner, now_instant);
                    if bus.running() {
                        bus_started = Some(now_instant);
                    }
                }
                let bus_ready = bus.running()
                    && (spawner.healthy(Service::Bus) || bus_started.is_none_or(|t| now_instant - t >= BUS_READY_WAIT));
                if bus_ready && !management.running() {
                    management.start(spawner, now_instant);
                }
                if bus.running() && management.running() {
                    if let Some(seq) = pending_seq.take() {
                        ack_seq = seq;
                    }
                }
            }
            Desired::Stopped | Desired::Exit => {
                management.stop(STOP_GRACE);
                bus.stop(STOP_GRACE);
                if let Some(seq) = pending_seq.take() {
                    ack_seq = seq;
                }
            }
        }
        let status = Status {
            schema: STATUS_SCHEMA.into(),
            pid: std::process::id(),
            version: crate::build_info::version_line().to_string(),
            started_at: started_at.clone(),
            heartbeat_at: now(),
            desired,
            ack_seq,
            bus: bus.status.clone(),
            management: management.status.clone(),
        };
        let changed = last_written.as_ref().is_none_or(|(at, last)| {
            now_instant - *at >= HEARTBEAT
                || last.ack_seq != status.ack_seq
                || last.bus != status.bus
                || last.management != status.management
                || last.desired != status.desired
        });
        if changed {
            if let Err(error) =
                crate::config::atomic::write_private_pretty_json_atomic(&status_path(layout), &status, "supervisor status")
            {
                eprintln!("cutex supervisor: cannot write status: {error:#}");
            }
            last_written = Some((now_instant, status));
        }
        if desired == Desired::Exit {
            break;
        }
        ticks += 1;
        if max_ticks.is_some_and(|max| ticks >= max) {
            break;
        }
        std::thread::sleep(TICK);
    }
    let _ = fs2::FileExt::unlock(&lock);
    Ok(())
}

// ------------------------------------------------------------ real spawner

/// Spawns the layout's service executable.
pub struct ProcessSpawner {
    pub exe: PathBuf,
    pub base: PathBuf,
    pub log_dir: PathBuf,
    pub bus_port: u16,
    pub management_port: u16,
}

impl ProcessSpawner {
    pub fn for_layout(layout: &Layout) -> anyhow::Result<Self> {
        let config = crate::config::store::load_codez_config_checked().unwrap_or_default();
        Ok(Self {
            exe: layout.service_executable(),
            base: layout.base.clone(),
            log_dir: crate::config::paths::runtime_dir()?.join("services"),
            bus_port: crate::agent_bus::service::agent_bus_port(&config),
            management_port: crate::management::service::management_port(&config),
        })
    }

    fn command(&self, service: Service) -> std::process::Command {
        let mut command = std::process::Command::new(&self.exe);
        command
            .args(service.args())
            .env(super::service_manager::SERVICE_MANAGER_ENV, super::service_manager::SUPERVISOR)
            .env(super::layout::INSTALL_BASE_ENV, &self.base);
        command
    }

    fn log(&self, service: Service) -> anyhow::Result<std::fs::File> {
        std::fs::create_dir_all(&self.log_dir)?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_dir.join(service.log_name()))
            .context("open service log")
    }
}

#[cfg(unix)]
struct UnixChild(std::process::Child);

#[cfg(unix)]
impl Child for UnixChild {
    fn id(&self) -> u32 {
        self.0.id()
    }
    fn poll_exit(&mut self) -> Option<String> {
        match self.0.try_wait() {
            Ok(Some(status)) => Some(format!("exited ({status})")),
            Ok(None) => None,
            Err(error) => Some(format!("lost ({error})")),
        }
    }
    fn stop(&mut self, grace: Duration) {
        unsafe {
            libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.0.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(windows)]
struct WindowsChild(crate::platform::windows_spawn::DetachedChild);

#[cfg(windows)]
impl Child for WindowsChild {
    fn id(&self) -> u32 {
        self.0.id()
    }
    fn poll_exit(&mut self) -> Option<String> {
        match self.0.try_wait() {
            Ok(Some(status)) => Some(format!("exited ({status})")),
            Ok(None) => None,
            Err(error) => Some(format!("lost ({error:#})")),
        }
    }
    fn stop(&mut self, grace: Duration) {
        // No console to signal (CREATE_NO_WINDOW children): terminate this
        // exact process; the Management API persists its state on every write.
        let _ = self.0.terminate();
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.0.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// A process recorded by an earlier supervisor: watched by PID.
struct AdoptedChild {
    pid: u32,
}

impl Child for AdoptedChild {
    fn id(&self) -> u32 {
        self.pid
    }
    fn poll_exit(&mut self) -> Option<String> {
        (!crate::platform::process::process_is_running(self.pid)).then(|| "exited (adopted)".to_string())
    }
    fn stop(&mut self, grace: Duration) {
        terminate_pid(self.pid);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline && crate::platform::process::process_is_running(self.pid) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Terminate exactly `pid` (no tree). Only used on PIDs this supervisor
/// recorded together with their creation time.
fn terminate_pid(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

impl Spawner for ProcessSpawner {
    fn spawn(&self, service: Service) -> anyhow::Result<Box<dyn Child>> {
        let log = self.log(service)?;
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let mut command = self.command(service);
            command.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
            unsafe {
                command.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
            Ok(Box::new(UnixChild(command.spawn().with_context(|| format!("start {}", self.exe.display()))?)))
        }
        #[cfg(windows)]
        {
            let child = crate::platform::windows_spawn::spawn_detached(&self.command(service), &log)?;
            Ok(Box::new(WindowsChild(child)))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = log;
            bail!("unsupported platform")
        }
    }

    fn healthy(&self, service: Service) -> bool {
        let port = match service {
            Service::Bus => self.bus_port,
            Service::Management => self.management_port,
        };
        crate::config::instance::ensure_port_allowed(port).is_ok()
            && std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(500)).is_ok()
    }

    fn adopt(&self, service: Service, pid: u32, process_started_at: Option<&str>) -> Option<Box<dyn Child>> {
        if !crate::platform::process::process_is_running(pid) || !self.healthy(service) {
            return None;
        }
        // PID reuse: the creation time must match what was recorded.
        let recorded = process_started_at?;
        (self.process_started_at(pid).as_deref() == Some(recorded)).then(|| Box::new(AdoptedChild { pid }) as Box<dyn Child>)
    }

    fn process_started_at(&self, pid: u32) -> Option<String> {
        crate::platform::process::process_started_at(pid).ok().map(|at| at.to_rfc3339())
    }
}

/// Windows: detach from the console the task gave us (no visible window).
pub fn detach_console() {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::FreeConsole();
    }
}

/// `cutex service run` entry point.
pub fn run(layout: &Layout) -> anyhow::Result<()> {
    if !layout.is_managed() {
        bail!("{} is not a managed install (run `cutex service install` first)", layout.base.display());
    }
    detach_console();
    let spawner = ProcessSpawner::for_layout(layout)?;
    run_loop(layout, &spawner, None)
}

pub fn log_dir_hint() -> Option<PathBuf> {
    crate::config::paths::runtime_dir().ok().map(|dir| dir.join("services"))
}

#[doc(hidden)]
pub fn _path_hint(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct World {
        next_pid: AtomicU32,
        alive: Mutex<std::collections::BTreeMap<u32, (Service, bool)>>,
        bus_healthy: AtomicBool,
        spawns: Mutex<Vec<Service>>,
        stops: Mutex<Vec<u32>>,
        fail_management_spawn: AtomicBool,
    }

    struct FakeChild {
        pid: u32,
        world: Arc<World>,
    }

    impl Child for FakeChild {
        fn id(&self) -> u32 {
            self.pid
        }
        fn poll_exit(&mut self) -> Option<String> {
            let alive = self.world.alive.lock().unwrap();
            (!alive.get(&self.pid).is_some_and(|(_, a)| *a)).then(|| "exited (1)".to_string())
        }
        fn stop(&mut self, _grace: Duration) {
            self.world.stops.lock().unwrap().push(self.pid);
            if let Some(entry) = self.world.alive.lock().unwrap().get_mut(&self.pid) {
                entry.1 = false;
            }
        }
    }

    struct FakeSpawner(Arc<World>);

    impl Spawner for FakeSpawner {
        fn spawn(&self, service: Service) -> anyhow::Result<Box<dyn Child>> {
            if service == Service::Management && self.0.fail_management_spawn.load(Ordering::SeqCst) {
                bail!("boom");
            }
            let pid = self.0.next_pid.fetch_add(1, Ordering::SeqCst) + 100;
            self.0.alive.lock().unwrap().insert(pid, (service, true));
            self.0.spawns.lock().unwrap().push(service);
            Ok(Box::new(FakeChild { pid, world: self.0.clone() }))
        }
        fn healthy(&self, service: Service) -> bool {
            service == Service::Management || self.0.bus_healthy.load(Ordering::SeqCst)
        }
        fn adopt(&self, _service: Service, pid: u32, started: Option<&str>) -> Option<Box<dyn Child>> {
            let alive = self.0.alive.lock().unwrap().get(&pid).is_some_and(|(_, a)| *a);
            (alive && started == Some("t0")).then(|| Box::new(FakeChild { pid, world: self.0.clone() }) as Box<dyn Child>)
        }
        fn process_started_at(&self, _pid: u32) -> Option<String> {
            Some("t0".into())
        }
    }

    fn layout() -> Layout {
        let dir = std::env::temp_dir().join(format!("cutex-supervisor-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        Layout::at(dir)
    }

    fn spawn_loop(layout: &Layout, world: &Arc<World>) -> std::thread::JoinHandle<anyhow::Result<()>> {
        let (layout, world) = (layout.clone(), world.clone());
        std::thread::spawn(move || run_loop(&layout, &FakeSpawner(world), Some(400)))
    }

    fn wait(cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn starts_bus_before_management_restarts_and_stops_on_request() {
        let layout = layout();
        let world = Arc::new(World::default());
        let handle = spawn_loop(&layout, &world);
        wait(|| world.spawns.lock().unwrap().len() == 1);
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(*world.spawns.lock().unwrap(), vec![Service::Bus], "management waits for the bus");
        world.bus_healthy.store(true, Ordering::SeqCst);
        wait(|| world.spawns.lock().unwrap().len() == 2);
        wait(|| read_status(&layout).is_some_and(|s| s.management.state == "running"));
        assert!(running(&layout));
        // Management exits: restarted after a backoff.
        let pid = read_status(&layout).unwrap().management.pid.unwrap();
        world.alive.lock().unwrap().get_mut(&pid).unwrap().1 = false;
        wait(|| read_status(&layout).is_some_and(|s| s.management.restarts == 1 && s.management.state == "running"));
        assert!(read_status(&layout).unwrap().management.last_exit.unwrap().contains("exited"));
        // Stop: management first, then the bus; acknowledged.
        let seq = request(&layout, Desired::Stopped, "test").unwrap();
        let status = wait_ack(&layout, seq, Duration::from_secs(20)).unwrap();
        assert_eq!((status.bus.state.as_str(), status.management.state.as_str()), ("stopped", "stopped"));
        let stops = world.stops.lock().unwrap().clone();
        let services: Vec<Service> = stops.iter().map(|pid| world.alive.lock().unwrap()[pid].0).collect();
        assert_eq!(services, vec![Service::Management, Service::Bus]);
        // Running again.
        let seq = request(&layout, Desired::Running, "test").unwrap();
        let status = wait_ack(&layout, seq, Duration::from_secs(20)).unwrap();
        assert_eq!(status.management.state, "running");
        let seq = request(&layout, Desired::Exit, "test").unwrap();
        handle.join().unwrap().unwrap();
        assert!(read_status(&layout).unwrap().ack_seq >= seq);
        assert!(!running(&layout));
        std::fs::remove_dir_all(&layout.base).unwrap();
    }

    #[test]
    fn single_instance_and_adoption() {
        let layout = layout();
        let world = Arc::new(World::default());
        world.bus_healthy.store(true, Ordering::SeqCst);
        let handle = spawn_loop(&layout, &world);
        wait(|| read_status(&layout).is_some_and(|s| s.management.state == "running"));
        let error = run_loop(&layout, &FakeSpawner(world.clone()), Some(1)).unwrap_err();
        assert!(error.to_string().contains("already runs"));
        // Simulate a supervisor crash: children live on, the next one adopts them.
        request(&layout, Desired::Exit, "test").unwrap();
        handle.join().unwrap().unwrap();
        // (Exit stopped them; bring two "survivors" back as if it had crashed.)
        let mut status = read_status(&layout).unwrap();
        for (slot, service) in [(&mut status.bus, Service::Bus), (&mut status.management, Service::Management)] {
            let pid = world.next_pid.fetch_add(1, Ordering::SeqCst) + 100;
            world.alive.lock().unwrap().insert(pid, (service, true));
            *slot = ChildStatus { state: "running".into(), pid: Some(pid), process_started_at: Some("t0".into()), ..Default::default() };
        }
        let control = read_control(&layout).unwrap();
        status.ack_seq = control.seq;
        crate::config::atomic::write_private_pretty_json_atomic(&status_path(&layout), &status, "test").unwrap();
        let spawned_before = world.spawns.lock().unwrap().len();
        let handle = spawn_loop(&layout, &world);
        wait(|| read_status(&layout).is_some_and(|s| s.management.adopted && s.bus.adopted));
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(world.spawns.lock().unwrap().len(), spawned_before, "adopted, not respawned");
        request(&layout, Desired::Exit, "test").unwrap();
        handle.join().unwrap().unwrap();
        std::fs::remove_dir_all(&layout.base).unwrap();
    }

    #[test]
    fn spawn_failures_back_off() {
        let layout = layout();
        let world = Arc::new(World::default());
        world.bus_healthy.store(true, Ordering::SeqCst);
        world.fail_management_spawn.store(true, Ordering::SeqCst);
        let handle = spawn_loop(&layout, &world);
        wait(|| read_status(&layout).is_some_and(|s| s.management.state == "backoff"));
        assert!(read_status(&layout).unwrap().management.last_exit.unwrap().contains("spawn failed"));
        world.fail_management_spawn.store(false, Ordering::SeqCst);
        wait(|| read_status(&layout).is_some_and(|s| s.management.state == "running"));
        request(&layout, Desired::Exit, "test").unwrap();
        handle.join().unwrap().unwrap();
        std::fs::remove_dir_all(&layout.base).unwrap();
    }
}
