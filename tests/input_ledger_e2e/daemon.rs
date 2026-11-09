//! Private debug daemon for the input-ledger E2E (054 §5.1 L2): a fresh
//! marked home per test under the user profile, `management.inputLedger.
//! enabled`, non-default ports, seeded sessions bound to fake runtimes, and a
//! recorder on the session + host event streams. Only the processes started
//! here (recorded PIDs) are ever stopped; the live daemon and `~/.cutex` are
//! never touched.
#![allow(dead_code)]

use crate::fake_runtime::{FakeRuntime, Kind};
use crate::trace::{now_ms, Trace, TraceEntry, DELIVERY_CHANGED, STATE_CHANGED};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Ports of the live install and other dev daemons: never used here.
pub const FORBIDDEN_PORTS: &[u16] = &[24270, 24260, 24250, 24670, 24660, 25370, 25360];
const MANAGEMENT_PORTS: std::ops::Range<u16> = 24370..24400;
const BUS_PORTS: std::ops::Range<u16> = 25470..25500;
pub const SLEEPER_ENV: &str = "INPUT_LEDGER_E2E_SLEEPER";
/// The fake's port a Codex sleeper forwards to (see [`forward_loopback`]).
pub const SLEEPER_FORWARD_ENV: &str = "INPUT_LEDGER_E2E_SLEEPER_FORWARD";
const SLEEPER_PORT_MARKER: &str = "INPUT_LEDGER_E2E_SLEEPER_PORT=";
#[cfg(unix)]
pub const SLEEPER_UNIX_ENV: &str = "INPUT_LEDGER_E2E_SLEEPER_UNIX";
#[cfg(unix)]
const SLEEPER_UNIX_MARKER: &str = "INPUT_LEDGER_E2E_SLEEPER_UNIX=";
/// Agent group of every seeded agent (`cutex agent send` resolves targets
/// inside the sender's groups).
pub const AGENT_GROUP: &str = "il-e2e";
/// The seeded Claude profile with `delegation_wait` on (S16 claude).
pub const DELEGATION_WAIT_PROFILE: &str = "il-e2e-claude-wait";

static PORT_CURSOR: AtomicUsize = AtomicUsize::new(usize::MAX);

/// A port of `range` that is free now: probed by binding it on the IPv4
/// loopback only (the address the daemon binds in tests; any non-loopback
/// bind makes Windows Firewall ask the owner for admin approval, once per
/// exe path) plus a connect, which also sees a wildcard listener another
/// process holds on the port, starting at a per-process random offset so
/// concurrent harness processes rarely probe the same ports. The probe is
/// released before the daemon binds it: [`Daemon::start`] retries with
/// fresh ports when a daemon still loses the race (os error 10048).
fn free_port(range: std::ops::Range<u16>) -> u16 {
    let len = range.len();
    let _ = PORT_CURSOR.compare_exchange(usize::MAX, (std::process::id() as usize).wrapping_mul(7919) ^ uuid::Uuid::new_v4().as_u128() as usize % len,
        Ordering::Relaxed, Ordering::Relaxed);
    for _ in 0..len * 2 {
        let n = PORT_CURSOR.fetch_add(1, Ordering::Relaxed);
        let port = range.start + (n % len) as u16;
        assert!(!FORBIDDEN_PORTS.contains(&port));
        let free = TcpListener::bind(("127.0.0.1", port)).is_ok()
            && TcpStream::connect_timeout(&(std::net::Ipv4Addr::LOCALHOST, port).into(), Duration::from_millis(50)).is_err();
        if free {
            return port;
        }
    }
    // Windows TIME_WAIT can consume this small preferred range during a
    // complete serial suite. Ask the OS for another private loopback port.
    for _ in 0..64 {
        let listener=TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,0)).expect("private ephemeral port");
        let port=listener.local_addr().unwrap().port();
        drop(listener);
        if port>=1024 && !FORBIDDEN_PORTS.contains(&port)
            && TcpStream::connect_timeout(&(std::net::Ipv4Addr::LOCALHOST,port).into(),Duration::from_millis(50)).is_err() {
            return port;
        }
    }
    panic!("no private loopback port available (preferred {range:?})");
}

// ---------------------------------------------------------------------------
// Private home

pub struct PrivateHome {
    pub root: PathBuf,
    pub config_dir: PathBuf,
    pub workspace: PathBuf,
}

impl PrivateHome {
    /// `<CUTEX_TEST_PRIVATE_HOME>/runs/<name>-<random>`, marked. The base must
    /// itself be a marked private home under the user profile (owner-private
    /// ACLs on Windows; env.sh sets it up).
    pub fn create(name: &str) -> Self {
        let base = PathBuf::from(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("CUTEX_TEST_PRIVATE_HOME (source env.sh)"));
        assert!(base.join(".cutex-test-private-home").is_file(), "{} is not a marked private home", base.display());
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            let profile = PathBuf::from(profile);
            assert!(base.starts_with(&profile) && base != profile, "the private home must be inside the user profile");
            assert!(!base.starts_with(profile.join(".cutex")));
        }
        let root = base.join("runs").join(format!("{name}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        #[cfg(unix)]
        let root = base.join(format!("r{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".cutex-test-private-home"), "input_ledger_e2e private home\n").unwrap();
        let config_dir = root.join(".cutex");
        let workspace = config_dir.join("workspace");
        for dir in [&workspace, &root.join("tmp"), &root.join("logs")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        Self { root, config_dir, workspace }
    }

    pub fn ledger_file(&self) -> PathBuf {
        self.config_dir.join("runtime").join("management-v2").join("input-ledger.sqlite3")
    }
}

// ---------------------------------------------------------------------------
// Seeded sessions bound to fake runtimes

/// A process whose PID stands for the runtime (`runtime_pid`, binding `pid`):
/// the boot fence wants a live PID started close to `started_at`.
///
/// For Codex it also serves the binding's endpoint ([`Sleeper::forwarding`]):
/// a reconnect verifies that the endpoint's listener is owned by the bound PID
/// (r273.1, `verify_exact_live_runtime_claim`: Windows TCP table owner), and
/// the fake's own listener belongs to the test process.
pub struct Sleeper {
    child: Child,
    endpoint: Option<String>,
    /// Kept open so the forwarder never writes to a closed pipe.
    _stdout: Option<BufReader<std::process::ChildStdout>>,
    #[cfg(unix)]
    socket_path: Option<PathBuf>,
}

impl Sleeper {
    fn command() -> Command {
        let exe = std::env::current_exe().unwrap();
        let mut command = Command::new(exe);
        command
            .args(["--exact", "harness_sleeper_child", "--ignored", "--nocapture", "--test-threads=1"])
            .env(SLEEPER_ENV, "1")
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    pub fn spawn() -> Self {
        let child = Self::command().stdout(Stdio::null()).spawn().expect("spawn sleeper");
        Self { child, endpoint: None, _stdout: None, #[cfg(unix)] socket_path: None }
    }

    /// A sleeper that listens on a loopback port of its own and forwards each
    /// connection to the fake runtime on `target` ([`forward_loopback`]).
    #[cfg(not(unix))]
    pub fn forwarding(target: u16) -> Self {
        let mut child = Self::command()
            .env(SLEEPER_FORWARD_ENV, target.to_string())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn forwarding sleeper");
        let mut stdout = BufReader::new(child.stdout.take().expect("sleeper stdout"));
        let mut line = String::new();
        let port = loop {
            line.clear();
            let read = stdout.read_line(&mut line).expect("read sleeper stdout");
            if read == 0 {
                let _ = child.kill();
                let _ = child.wait();
                panic!("forwarding sleeper exited before it reported its port");
            }
            // libtest prints `test harness_sleeper_child ... ` on the same line.
            if let Some((_, port)) = line.trim().split_once(SLEEPER_PORT_MARKER) {
                break port.parse::<u16>().expect("sleeper port");
            }
        };
        Self { child, endpoint: Some(format!("ws://127.0.0.1:{port}")), _stdout: Some(stdout) }
    }

    #[cfg(unix)]
    pub fn forwarding_in(target: u16, runtime_dir: PathBuf) -> Self {
        std::fs::create_dir_all(&runtime_dir).unwrap();
        let path = runtime_dir.join("rpc.sock");
        assert!(path.as_os_str().as_encoded_bytes().len() < 108, "use a shorter marked E2E base");
        let mut child = Self::command().env(SLEEPER_FORWARD_ENV, target.to_string())
            .env(SLEEPER_UNIX_ENV, &path).stdout(Stdio::piped()).spawn().expect("spawn Unix forwarding sleeper");
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            if stdout.read_line(&mut line).unwrap() == 0 {
                let _ = child.kill(); let _ = child.wait();
                panic!("Unix forwarding sleeper exited before its listener was ready");
            }
            if let Some((_, reported)) = line.trim().split_once(SLEEPER_UNIX_MARKER) {
                assert_eq!(Path::new(reported), path);
                break;
            }
        }
        Self { child, endpoint: Some(format!("unix://{}", path.display())), _stdout: Some(stdout), socket_path: Some(path) }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The endpoint this sleeper serves (forwarding sleepers only).
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        #[cfg(unix)]
        if let Some(path) = &self.socket_path { let _ = std::fs::remove_file(path); }
    }
}

/// Sleeper side of [`Sleeper::forwarding`]: a loopback listener owned by
/// this process that relays every connection, both ways, to `target`. A close
/// on either side closes both (the fake's `disconnect_all` drops the daemon's
/// link; a refused fake connection closes the daemon's at once). Exits after
/// the same 3 h as a plain sleeper if nobody kills it.
pub fn forward_loopback(target: u16) -> ! {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(3 * 3600));
        std::process::exit(0);
    });
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind sleeper forwarder");
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{SLEEPER_PORT_MARKER}{}", listener.local_addr().unwrap().port()).unwrap();
    stdout.flush().unwrap();
    for outer in listener.incoming() {
        let Ok(outer) = outer else { continue };
        std::thread::spawn(move || {
            let Ok(inner) = TcpStream::connect(("127.0.0.1", target)) else { return };
            let _ = (outer.set_nodelay(true), inner.set_nodelay(true));
            let (Ok(outer_read), Ok(inner_read)) = (outer.try_clone(), inner.try_clone()) else { return };
            let relay = |from: TcpStream, to: TcpStream| {
                let _ = std::io::copy(&mut &from, &mut &to);
                let _ = from.shutdown(Shutdown::Both);
                let _ = to.shutdown(Shutdown::Both);
            };
            let upstream = std::thread::spawn(move || relay(outer_read, inner));
            relay(inner_read, outer);
            let _ = upstream.join();
        });
    }
    std::process::exit(0)
}

/// The bound child owns this Unix listener, so Linux's real SO_PEERCRED
/// verification observes the same PID stored in the session binding.
#[cfg(unix)]
pub fn forward_unix(target: u16, path: &Path) -> ! {
    use std::os::unix::net::UnixListener;
    assert!(path.is_absolute());
    std::thread::spawn(|| { std::thread::sleep(Duration::from_secs(3 * 3600)); std::process::exit(0); });
    let listener = UnixListener::bind(path).expect("bind owned sleeper Unix forwarder");
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{SLEEPER_UNIX_MARKER}{}", path.display()).unwrap(); stdout.flush().unwrap();
    for outer in listener.incoming() {
        let Ok(mut outer) = outer else { continue };
        std::thread::spawn(move || {
            let Ok(mut inner) = TcpStream::connect(("127.0.0.1", target)) else { return };
            let _ = inner.set_nodelay(true);
            let (Ok(mut outer_read), Ok(mut inner_read)) = (outer.try_clone(), inner.try_clone()) else { return };
            let upstream = std::thread::spawn(move || {
                let _ = std::io::copy(&mut outer_read, &mut inner);
                let _ = outer_read.shutdown(Shutdown::Both); let _ = inner.shutdown(Shutdown::Both);
            });
            let _ = std::io::copy(&mut inner_read, &mut outer);
            let _ = inner_read.shutdown(Shutdown::Both); let _ = outer.shutdown(Shutdown::Both);
            let _ = upstream.join();
        });
    }
    std::process::exit(0)
}

pub struct SeededAgent {
    pub sid: String,
    pub kind: Kind,
    pub runtime_agent_id: String,
    pub fake: FakeRuntime,
    pub sleeper: Sleeper,
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Session records the daemon imports on first start
/// (`cutex-sessions.json`), shaped like tests/queue_order_smoke.py.
pub fn seed_sessions(home: &PrivateHome, agents: &[(&SeededAgentSpec, &FakeRuntime, &Sleeper)]) -> Vec<String> {
    let host = cutex::platform::host::current_host_name();
    let mut sessions = serde_json::Map::new();
    let mut ids = Vec::new();
    for (index, (spec, fake, sleeper)) in agents.iter().enumerate() {
        let sid = format!("cutex.{}", uuid::Uuid::new_v4());
        let state = home.config_dir.join("runtime").join("app-server").join(format!("il-e2e-{index}-{}", fake.kind.name()));
        std::fs::create_dir_all(&state).unwrap();
        let rpc_token = state.join("rpc-token");
        std::fs::write(&rpc_token, uuid::Uuid::new_v4().simple().to_string()).unwrap();
        let pid = sleeper.pid();
        let mut record = json!({
            "cutex_session_id": sid, "host_id": host, "cwd": home.workspace.display().to_string(),
            "created_at": now_rfc3339(), "updated_at": now_rfc3339(),
            "runtime_pid": pid, "runtime_generation": 1, "runtime_history_known": true,
            "exposed_to_backend": true, "current_runtime_agent_id": spec.runtime_agent_id,
            "registration_class": if spec.persistent { "persistent" } else { "local_only" },
            "agent_enabled": spec.persistent, "agent_groups": [AGENT_GROUP],
        });
        if let Some(name) = &spec.thread_name {
            record["thread_name"] = json!(name);
        }
        if let Some(profile) = &spec.profile {
            record["profile"] = json!(profile);
        }
        match fake.kind {
            Kind::Codex => {
                record["codex_session_id"] = json!(fake.thread);
                record["runtime_backend"] = json!("host");
                record["app_server_runtime"] = json!({
                    "transport": "loopback_web_socket", "endpoint": sleeper.endpoint().unwrap_or(&fake.url), "pid": pid,
                    "runtime_dir": state.display().to_string(),
                    "auth_token_path": rpc_token.display().to_string(),
                    "diagnostic_journal_path": state.join("events.jsonl").display().to_string(),
                    "schema_version": "input-ledger-e2e-fixture", "schema_sha256": "input-ledger-e2e-fixture",
                    "started_at": now_rfc3339(),
                });
                #[cfg(unix)]
                { record["app_server_runtime"]["transport"] = json!("unix_socket"); }
            }
            Kind::Claude => {
                let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("components").join("claude-runtime");
                let binary = env!("CARGO_BIN_EXE_cutex");
                record["runtime_backend"] = json!("claude_subscription");
                record["claude_session"] = json!({
                    "deployment": {"python": binary, "adapter_source": source.display().to_string(),
                        "claude_cli": binary, "tui_binary": binary, "claude_home": home.config_dir.display().to_string()},
                    "thread_id": fake.thread, "state_dir": state.display().to_string(), "model": "fake",
                    "native_session_id": "fixture-native", "connection_id": fake.connection_id,
                });
                std::fs::write(state.join("server.json"),
                    json!({"endpoint": fake.url, "runtimeId": "fixture-claude", "pid": pid}).to_string()).unwrap();
            }
        }
        sessions.insert(sid.clone(), record);
        ids.push(sid);
    }
    if agents.iter().any(|(spec, _, _)| spec.profile.is_some()) {
        // accounts.json (profiles/model.rs `AccountsStore`, version 3): one
        // Claude profile with `options.delegation_wait` on, no credentials.
        let store = json!({"version": 3, "active_account_id": null, "accounts": [{
            "id": DELEGATION_WAIT_PROFILE, "name": DELEGATION_WAIT_PROFILE, "email": null, "plan_type": null,
            "last_used_at": null, "cli_kind": "claude", "options": {"delegation_wait": true}}]});
        std::fs::write(home.config_dir.join("accounts.json"), serde_json::to_vec_pretty(&store).unwrap()).unwrap();
    }
    std::fs::write(home.config_dir.join("cutex-sessions.json"), serde_json::to_vec_pretty(&json!({"sessions": sessions})).unwrap()).unwrap();
    ids
}

#[derive(Debug, Clone)]
pub struct SeededAgentSpec {
    pub kind: Kind,
    pub runtime_agent_id: String,
    pub thread_name: Option<String>,
    /// A persistent agent (`agent_enabled`, registration class persistent):
    /// the delegation tools accept only such a caller (agent_bus/server.rs
    /// `persistent_agent_occurrence`).
    pub persistent: bool,
    /// The session's `profile` (a profile `seed_sessions` writes into the
    /// home's accounts.json: Claude, `delegation_wait` on).
    pub profile: Option<String>,
}

// ---------------------------------------------------------------------------
// Daemon

#[derive(Debug, Clone)]
pub struct DaemonOptions {
    /// Exact previous debug build for private compatibility probes only.
    pub executable: Option<PathBuf>,
    pub ledger_enabled: bool,
    /// Also run the Agent Bus (`cutex agent serve`) for wakes, notes, alarms.
    pub bus: bool,
    /// `CUTEX_TEST_FAKE_EXTERNAL_INPUT=1` (C3's debug hook): bus messages to a
    /// seeded app-server session go over the fake's
    /// `thread/externalInput/submit|status` (no reviewed stock bundle).
    pub fake_external_input: bool,
    /// `agent_bus.wake_busy_wait_secs` (None: the default wait).
    pub wake_busy_wait_secs: Option<u64>,
    /// Daemon-to-daemon peers on (`peers.enabled`; S24 pairs two private
    /// daemons over loopback through `/v2/peers`).
    pub peers: bool,
    /// `CUTEX_TEST_TURN_STALL_SECS` (debug hook, turn_stall.rs): a bound
    /// turn with no activity for this many seconds gets its stall check
    /// (default 5 min).
    pub turn_stall_secs: Option<u64>,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self { executable: None, ledger_enabled: true, bus: false, fake_external_input: false, wake_busy_wait_secs: None, peers: false, turn_stall_secs: None }
    }
}

pub struct Daemon {
    pub home: PrivateHome,
    pub port: u16,
    pub bus_port: u16,
    pub token: String,
    pub bus_token: String,
    pub options: DaemonOptions,
    management: Option<Child>,
    bus: Option<Child>,
    /// Ports stay fixed once a daemon was up (recorders, restarts).
    started_once: bool,
}

impl Daemon {
    pub fn configure(home: PrivateHome, mut options: DaemonOptions) -> Self {
        if options.executable.is_none() {
            if let Some(path) = std::env::var_os("INPUT_LEDGER_E2E_DAEMON") {
                let path = PathBuf::from(path); assert!(path.is_absolute() && path.is_file());
                options.executable = Some(path);
            }
        }
        let port = free_port(MANAGEMENT_PORTS);
        let bus_port = free_port(BUS_PORTS);
        let token = uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string();
        let bus_token = uuid::Uuid::new_v4().simple().to_string();
        let daemon = Self { home, port, bus_port, token, bus_token, options, management: None, bus: None, started_once: false };
        daemon.write_config();
        daemon
    }

    fn write_config(&self) {
        let (port, bus_port) = (self.port, self.bus_port);
        let mut config = json!({
            "agent_bus_enabled": self.options.bus, "agent_bus_port": bus_port, "agent_bus_token": self.bus_token,
            "management_port": port, "management_api_token": self.token, "agent_management_admin_token": self.token,
            "management_listen": [format!("127.0.0.1:{port}")],
            "usage": {"priceSource": "manual"},
            "management": {"inputLedger": {"enabled": self.options.ledger_enabled}},
        });
        if self.options.bus {
            // Seeded agents belong to no project: host-wide visibility, the
            // agent group decides (agent_bus/boundary.rs; delegations would
            // be `recipient_outside_boundary`).
            config["agent_security"] = json!({"cross_project": "host"});
        }
        if let Some(secs) = self.options.wake_busy_wait_secs {
            config["agent_bus"] = json!({"wake_busy_wait_secs": secs});
        }
        if self.options.peers {
            config["peers"] = json!({"enabled": true});
        }
        std::fs::write(self.home.config_dir.join("config.json"), serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    }

    /// `agent_bus_token` as this daemon's own config.json holds it (the
    /// private home's file, never `~/.cutex`).
    pub fn bus_token_from_config(&self) -> String {
        let config: Value = serde_json::from_slice(&std::fs::read(self.home.config_dir.join("config.json")).unwrap()).unwrap();
        config["agent_bus_token"].as_str().map(str::to_string).unwrap_or_else(|| self.bus_token.clone())
    }

    /// Environment of every `cutex` child: the private home only.
    pub fn command(&self, args: &[&str]) -> Command {
        let program=self.options.executable.as_deref().unwrap_or_else(|| Path::new(env!("CARGO_BIN_EXE_cutex")));
        self.command_for(program.to_str().expect("private executable path"), args)
    }

    pub fn command_for(&self, program: &str, args: &[&str]) -> Command {
        let mut command = Command::new(program);
        let root = self.home.root.as_os_str();
        let tmp = self.home.root.join("tmp");
        command
            .args(args)
            .current_dir(&self.home.workspace)
            .env_remove("CUTEX_HOME")
            .env_remove("CUTEX_TEST_FAKE_EXTERNAL_INPUT")
            .env_remove("CUTEX_TEST_TURN_STALL_SECS")
            .env("HOME", root)
            .env("CUTEX_TEST_PRIVATE_HOME", root)
            .env("CUTEX_TEST_FORBID_REAL_HOME", "1")
            .env("TEMP", &tmp)
            .env("TMP", &tmp)
            .env("TMPDIR", &tmp)
            .env("HOSTNAME", cutex::platform::host::current_host_name())
            .env("CUTEX_MANAGEMENT_URL", format!("http://127.0.0.1:{}", self.port))
            .stdin(Stdio::null());
        for (key, _) in std::env::vars_os() {
            let key = key.to_string_lossy().to_string();
            if key.starts_with("CUTEX_AGENT") || key == "CUTEX_RUNTIME_GENERATION" || key == "CUTEX_OBSERVER_URL" {
                command.env_remove(key);
            }
        }
        if self.options.fake_external_input {
            command.env("CUTEX_TEST_FAKE_EXTERNAL_INPUT", "1");
        }
        if let Some(secs) = self.options.turn_stall_secs {
            command.env("CUTEX_TEST_TURN_STALL_SECS", secs.to_string());
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        command
    }

    fn log(&self, name: &str) -> std::fs::File {
        std::fs::OpenOptions::new().create(true).append(true).open(self.home.root.join("logs").join(name)).unwrap()
    }

    /// Start the bus (if configured) and Management. A daemon that exits
    /// before it is ready (typically os error 10048: a probed port was taken
    /// meanwhile) is retried, with fresh ports on the first start.
    pub fn start(&mut self) {
        let attempts = 4;
        for attempt in 1..=attempts {
            match self.try_start() {
                Ok(()) => {
                    self.started_once = true;
                    return;
                }
                Err(why) if attempt < attempts => {
                    eprintln!("input_ledger_e2e: daemon start attempt {attempt} failed ({why}); retrying");
                    self.stop_management();
                    self.stop_bus();
                    if !self.started_once {
                        self.port = free_port(MANAGEMENT_PORTS);
                        self.bus_port = free_port(BUS_PORTS);
                        self.write_config();
                    }
                    std::thread::sleep(Duration::from_millis(500 * attempt as u64));
                }
                Err(why) => panic!("{why}; see {}", self.home.root.join("logs").display()),
            }
        }
    }

    fn try_start(&mut self) -> Result<(), String> {
        // Management starts a bus even when its optional bus feature is off.
        // Own that exact child here so every fixture can stop all its writers.
        if self.bus.is_none() {
            let port = self.bus_port.to_string();
            let log = self.log("bus.log");
            let child = self.command(&["agent", "serve", "--port", &port])
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .expect("start agent bus");
            self.bus = Some(child);
            let until = Instant::now() + Duration::from_secs(60);
            loop {
                if let Some(Ok(Some(status))) = self.bus.as_mut().map(Child::try_wait) {
                    self.bus = None;
                    return Err(format!("agent bus on {} exited ({status})", self.bus_port));
                }
                if TcpStream::connect_timeout(&(std::net::Ipv4Addr::LOCALHOST, self.bus_port).into(), Duration::from_millis(200)).is_ok()
                    && matches!(http_request(self.bus_port,&self.bus_token,"GET","/",None),Ok((200,_))) {
                    break;
                }
                if Instant::now() >= until {
                    return Err(format!("agent bus on {} not ready", self.bus_port));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        let port = self.port.to_string();
        let log = self.log("management.log");
        let child = self.command(&["management", "serve", "--port", &port])
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("start management");
        self.management = Some(child);
        let until = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(Ok(Some(status))) = self.management.as_mut().map(Child::try_wait) {
                self.management = None;
                return Err(format!("management on {} exited ({status})", self.port));
            }
            if matches!(self.http("GET", "/v2/sessions", None), Ok((200, _))) {
                if let Some(Ok(Some(status))) = self.bus.as_mut().map(Child::try_wait) {
                    self.bus = None;
                    return Err(format!("agent bus on {} exited ({status})", self.bus_port));
                }
                return Ok(());
            }
            if Instant::now() >= until {
                return Err("management not ready".into());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn stop_bus(&mut self) {
        if let Some(mut child) = self.bus.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// One MCP `tools/call` of `cutex-mcp` as a seeded agent (its runtime
    /// agent id and generation, the private daemon's bus URL and token),
    /// bound to `thread` (`_meta.threadId`), like
    /// tests/stock_task_mcp_boundary.py. Returns the parsed tool result
    /// (`content[0].text` as JSON when it is JSON) or the raw reply.
    pub fn mcp_call(&self, runtime_agent_id: &str, generation: u64, thread: &str, tool: &str, arguments: Value) -> Result<Value, String> {
        let program = option_env!("CARGO_BIN_EXE_cutex-mcp").map(PathBuf::from).unwrap_or_else(|| {
            Path::new(env!("CARGO_BIN_EXE_cutex")).with_file_name(if cfg!(windows) { "cutex-mcp.exe" } else { "cutex-mcp" })
        });
        let mut command = self.command_for(program.to_str().unwrap(), &[]);
        command
            .env("CUTEX_AGENT_ID", runtime_agent_id)
            .env("CUTEX_RUNTIME_GENERATION", generation.to_string())
            .env("CUTEX_AGENT_BUS_URL", format!("http://127.0.0.1:{}/", self.bus_port))
            .env("CUTEX_AGENT_BUS_TOKEN", self.bus_token_from_config())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments, "_meta": {"threadId": thread}}});
        let mut child = command.spawn().map_err(|e| format!("spawn cutex-mcp: {e}"))?;
        child.stdin.take().unwrap().write_all(format!("{call}\n").as_bytes()).map_err(|e| e.to_string())?;
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let reply: Value = stdout.lines().find_map(|line| serde_json::from_str(line).ok())
            .ok_or_else(|| format!("cutex-mcp {tool}: no reply (status {:?}); stdout {stdout:?} stderr {stderr:?}", out.status.code()))?;
        let text = reply.pointer("/result/content/0/text").and_then(Value::as_str);
        Ok(text.and_then(|t| serde_json::from_str(t).ok()).unwrap_or(reply))
    }
    pub fn management_pid(&self) -> Option<u32> {
        self.management.as_ref().map(Child::id)
    }

    /// Kill the Management process this harness started (only that PID).
    pub fn stop_management(&mut self) {
        if let Some(mut child) = self.management.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn stop_all(&mut self) {
        self.stop_management();
        self.stop_bus();
    }

    pub fn restart(&mut self) {
        self.stop_management();
        self.start();
    }

    pub fn http(&self, method: &str, path: &str, body: Option<&Value>) -> std::io::Result<(u16, Value)> {
        http_request(self.port, &self.token, method, path, body)
    }

    /// `POST /v2/sessions/{id}/cutex/requests`: `Ok(result)` or `Err(error body)`.
    pub fn cutex(&self, sid: &str, method: &str, params: Value) -> Result<Value, Value> {
        let body = json!({"requestId": uuid::Uuid::new_v4().to_string(), "method": method, "params": params});
        match self.http("POST", &format!("/v2/sessions/{sid}/cutex/requests"), Some(&body)) {
            Ok((200, value)) => Ok(value["cutex"]["result"].clone()),
            Ok((status, value)) => Err(json!({"status": status, "body": value})),
            Err(error) => Err(json!({"io": error.to_string()})),
        }
    }

    pub fn session(&self, sid: &str) -> Value {
        self.http("GET", &format!("/v2/sessions/{sid}"), None).map(|(_, v)| v).unwrap_or(Value::Null)
    }

    pub fn connected(&self, sid: &str) -> bool {
        self.session(sid)["runtime"]["appServerConnected"] == true
    }

    pub fn wait_connected(&self, sid: &str, timeout: Duration) {
        let until = Instant::now() + timeout;
        while !self.connected(sid) {
            assert!(Instant::now() < until, "{sid} never connected: {}; see {}", self.session(sid), self.home.root.join("logs").display());
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn active_turn(&self, sid: &str) -> Option<String> {
        self.session(sid)["runtime"]["activeTurnId"].as_str().map(str::to_string)
    }

    pub fn wait_active_turn(&self, sid: &str, active: bool, timeout: Duration) {
        let until = Instant::now() + timeout;
        while self.active_turn(sid).is_some() != active {
            assert!(Instant::now() < until, "{sid}: activeTurn never became {active}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// A `cutex` CLI call against this home (alarm set, agent send, ...).
    pub fn cli(&self, args: &[&str]) -> std::process::Output {
        self.command(args).output().expect("run cutex cli")
    }
}

/// Two private daemons paired as peers (`peers.enabled` on both).
#[derive(Debug, Clone)]
pub struct PeerPair {
    /// The receiver as the sender knows it (`name@<label>` addresses).
    pub receiver_label: String,
    /// The receiver's peer id on the sender.
    pub receiver_on_sender: String,
    /// The sender's peer id on the receiver (`record.remote.peerId`).
    pub sender_on_receiver: String,
}

#[track_caller]
fn peers_ok(daemon: &Daemon, method: &str, path: &str, body: Option<&Value>) -> Value {
    let (status, value) = daemon.http(method, path, body).unwrap_or_else(|e| panic!("{method} {path}: {e}"));
    assert!((200..300).contains(&status), "{method} {path} on :{}: {status} {value}", daemon.port);
    value
}

/// Pair `sender` and `receiver` the way the owner does it (the real
/// handshake, both over loopback, root credential = owner-local): the
/// receiver issues an invite code (`POST /v2/peers/invites`), the sender adds
/// it (`POST /v2/peers` → `/peer/v1/pair`, hello, confirm). Then policies:
/// the sender lists the receiver's agents (`outbound.visibility: all`, so
/// `agent_tell {to: "<agent>@<label>"}` resolves); the receiver exports
/// `receiver_agent` to the sender, accepts its input and its wakes without
/// review (`inbound.tellWake`, `review: never`).
pub fn pair_peers(sender: &Daemon, receiver: &Daemon, receiver_agent: &str) -> PeerPair {
    let invite = peers_ok(receiver, "POST", "/v2/peers/invites", Some(&json!({})));
    let code = invite["code"].as_str().unwrap_or_else(|| panic!("invite without code: {invite}"));
    let url = format!("http://127.0.0.1:{}", receiver.port);
    let added = peers_ok(sender, "POST", "/v2/peers", Some(&json!({"code": code, "url": url})));
    let peer = &added["peer"];
    let receiver_on_sender = peer["peerId"].as_str().unwrap_or_else(|| panic!("no peerId: {added}")).to_string();
    let receiver_label = peer["label"].as_str().unwrap_or_else(|| panic!("no label: {added}")).to_string();
    let listed = peers_ok(receiver, "GET", "/v2/peers", None);
    let sender_on_receiver = listed["peers"].as_array().and_then(|p| p.first()).and_then(|p| p["peerId"].as_str())
        .unwrap_or_else(|| panic!("the receiver lists no peer: {listed}")).to_string();
    peers_ok(sender, "PATCH", &format!("/v2/peers/{receiver_on_sender}"),
        Some(&json!({"policy": {"outbound": {"visibility": "all"}}})));
    peers_ok(receiver, "PATCH", &format!("/v2/peers/{sender_on_receiver}"),
        Some(&json!({"policy": {"inbound": {"exports": [{"agent": receiver_agent}], "accept": true,
            "tellWake": true, "review": "never"}}})));
    PeerPair { receiver_label, receiver_on_sender, sender_on_receiver }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop_management();
        if let Some(mut child) = self.bus.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------
// Raw HTTP/1.1 (loopback, bearer)

fn request_head(port: u16, token: &str, method: &str, path: &str, extra: &[(&str, String)], body_len: Option<usize>) -> String {
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n");
    for (key, value) in extra {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    if let Some(len) = body_len {
        head.push_str(&format!("Content-Type: application/json\r\nContent-Length: {len}\r\n"));
    }
    head
}

pub fn http_request(port: u16, token: &str, method: &str, path: &str, body: Option<&Value>) -> std::io::Result<(u16, Value)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let body = body.map(|b| b.to_string());
    let mut head = request_head(port, token, method, path, &[], body.as_ref().map(String::len));
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    if let Some(body) = &body {
        stream.write_all(body.as_bytes())?;
    }
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| std::io::Error::other("no HTTP head"))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut payload = raw[split + 4..].to_vec();
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        payload = dechunk(&payload);
    }
    let value = serde_json::from_slice(&payload).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&payload).to_string()));
    Ok((status, value))
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(end) = data.windows(2).position(|w| w == b"\r\n") else { break };
        let size = usize::from_str_radix(String::from_utf8_lossy(&data[..end]).trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = end + 2;
        out.extend_from_slice(&data[start..(start + size).min(data.len())]);
        data = &data[(start + size + 2).min(data.len())..];
    }
    out
}

// ---------------------------------------------------------------------------
// Recorder

struct Stream {
    stop: Arc<AtomicBool>,
    socket: Arc<Mutex<Option<TcpStream>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub struct Recorder {
    pub trace: Arc<Mutex<Trace>>,
    streams: Vec<Stream>,
}

impl Recorder {
    pub fn new() -> Self {
        Self { trace: Arc::new(Mutex::new(Trace::default())), streams: Vec::new() }
    }

    /// `source` = `session` (with `cutexSessionId`) or `host` (none). Follows
    /// daemon restarts with `Last-Event-ID`.
    pub fn subscribe(&mut self, daemon: &Daemon, source: &str, sid: Option<&str>) {
        self.subscribe_after(daemon, source, sid, None);
    }

    /// Start a separate observation phase at the real journal checkpoint.
    /// Offline fixture writes have no daemon event and must not be replayed
    /// as if they belonged to the subsequent live delivery sequence.
    pub fn subscribe_latest(&mut self, daemon: &Daemon, source: &str, sid: Option<&str>) {
        let mut checkpoint: Option<(String,String)> = None;
        loop {
            let mut query=url::form_urlencoded::Serializer::new(String::new());
            query.append_pair("limit","100");
            if let Some(sid)=sid {query.append_pair("cutexSessionId",sid);}
            if let Some((after,stream))=&checkpoint {
                query.append_pair("after",after).append_pair("streamId",stream);
            }
            let (status,page)=daemon.http("GET",&format!("/v2/events?{}",query.finish()),None).unwrap();
            assert_eq!(status,200,"event checkpoint: {page}");
            let stream=page["streamId"].as_str().unwrap().to_string();
            if let Some(after)=page["nextCursor"].as_str().or(page["checkpoint"]["cursor"].as_str()) {
                checkpoint=Some((after.to_string(),stream));
            }
            if page["hasMore"]!=true {break;}
        }
        self.subscribe_after(daemon,source,sid,checkpoint);
    }

    fn subscribe_after(&mut self, daemon: &Daemon, source: &str, sid: Option<&str>, initial: Option<(String,String)>) {
        let stop = Arc::new(AtomicBool::new(false));
        let socket = Arc::new(Mutex::new(None::<TcpStream>));
        let ready = Arc::new(AtomicBool::new(false));
        let (port, token) = (daemon.port, daemon.token.clone());
        let (source, sid) = (source.to_string(), sid.map(str::to_string));
        let (trace, stop_t, socket_t, ready_t) = (self.trace.clone(), stop.clone(), socket.clone(), ready.clone());
        let thread = std::thread::spawn(move || {
            let mut cursor: Option<(String, String)> = initial; // (cursor, streamId)
            while !stop_t.load(Ordering::Relaxed) {
                if let Err(error) = follow(port, &token, &source, sid.as_deref(), &mut cursor, &trace, &socket_t, &ready_t) {
                    if !stop_t.load(Ordering::Relaxed) {
                        push(&trace, &source, "note", sid.as_deref(), None, json!({"streamError": error.to_string()}));
                    }
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        let until = Instant::now() + Duration::from_secs(20);
        while !ready.load(Ordering::Relaxed) {
            assert!(Instant::now() < until, "event stream did not open");
            std::thread::sleep(Duration::from_millis(20));
        }
        self.streams.push(Stream { stop, socket, thread: Some(thread) });
    }

    pub fn snapshot(&self) -> Trace {
        self.trace.lock().unwrap().clone()
    }

    pub fn mark(&self, kind: &str, payload: Value) {
        push(&self.trace, "marker", kind, None, None, payload);
    }

    /// `cutex/input/list` into the trace (or a note when it is refused).
    pub fn read_input_list(&self, daemon: &Daemon, sid: &str) -> Result<Value, Value> {
        let result = daemon.cutex(sid, "cutex/input/list", json!({}));
        match &result {
            Ok(value) => push(&self.trace, "snapshot", "inputList", Some(sid), None, value.clone()),
            Err(error) => push(&self.trace, "snapshot", "note", Some(sid), None, json!({"inputListError": error})),
        }
        result
    }

    pub fn read_queue_list(&self, daemon: &Daemon, sid: &str) -> Result<Value, Value> {
        let result = daemon.cutex(sid, "cutex/userInput/queue/list", json!({}));
        match &result {
            Ok(value) => push(&self.trace, "snapshot", "queueList", Some(sid), None, value.clone()),
            Err(error) => push(&self.trace, "snapshot", "note", Some(sid), None, json!({"queueListError": error})),
        }
        result
    }

    pub fn stop(&mut self) -> Trace {
        for stream in &self.streams {
            stream.stop.store(true, Ordering::Relaxed);
            if let Some(socket) = stream.socket.lock().unwrap().as_ref() {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
        for stream in &mut self.streams {
            if let Some(thread) = stream.thread.take() {
                let _ = thread.join();
            }
        }
        self.snapshot()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop();
    }
}

fn push(trace: &Arc<Mutex<Trace>>, source: &str, kind: &str, sid: Option<&str>, sequence: Option<u64>, payload: Value) {
    trace.lock().unwrap().push(TraceEntry {
        at_ms: now_ms(),
        source: source.into(),
        kind: kind.into(),
        cutex_session_id: sid.map(str::to_string),
        sequence,
        payload,
    });
}

#[allow(clippy::too_many_arguments)]
fn follow(
    port: u16,
    token: &str,
    source: &str,
    sid: Option<&str>,
    cursor: &mut Option<(String, String)>,
    trace: &Arc<Mutex<Trace>>,
    socket: &Arc<Mutex<Option<TcpStream>>>,
    ready: &AtomicBool,
) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    let mut query = Vec::new();
    if let Some(sid) = sid {
        query.push(format!("cutexSessionId={sid}"));
    }
    let mut extra = vec![("Accept", "text/event-stream".to_string())];
    if let Some((after, stream_id)) = cursor.as_ref() {
        query.push(format!("streamId={stream_id}"));
        extra.push(("Last-Event-ID", after.clone()));
    }
    let path = if query.is_empty() { "/v2/events/stream".to_string() } else { format!("/v2/events/stream?{}", query.join("&")) };
    let mut head = request_head(port, token, "GET", &path, &extra, None);
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    *socket.lock().unwrap() = Some(stream.try_clone()?);
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    if !status.contains(" 200 ") {
        let mut rest = String::new();
        let _ = reader.read_to_string(&mut rest);
        if status.contains(" 409 ") {
            *cursor = None; // cursor expired: resubscribe from now
            push(trace, source, "resubscribed", sid, None, json!({"status": status.trim()}));
        }
        return Err(std::io::Error::other(format!("{} {}", status.trim(), rest.trim())));
    }
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
    }
    ready.store(true, Ordering::Relaxed);
    let (mut event, mut data, mut id) = (String::new(), String::new(), String::new());
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(std::io::Error::other("event stream ended"));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if !data.is_empty() {
                record_frame(source, sid, &event, &id, &data, cursor, trace);
            }
            event.clear();
            data.clear();
            id.clear();
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
        } else if let Some(value) = line.strip_prefix("id:") {
            id = value.trim().to_string();
        }
    }
}

fn record_frame(source: &str, sid: Option<&str>, event: &str, id: &str, data: &str, cursor: &mut Option<(String, String)>, trace: &Arc<Mutex<Trace>>) {
    let Ok(envelope) = serde_json::from_str::<Value>(data) else {
        return push(trace, source, "note", sid, None, json!({"event": event, "raw": data}));
    };
    if event != "cutex_management_event_v2" {
        return push(trace, source, "note", sid, None, json!({"event": event, "data": envelope}));
    }
    let stream_id = envelope["streamId"].as_str().unwrap_or("").to_string();
    let after = envelope["cursor"].as_str().map(str::to_string).unwrap_or_else(|| id.to_string());
    *cursor = Some((after, stream_id));
    let sequence = envelope["sequence"].as_u64();
    let session = envelope["cutexSessionId"].as_str().map(str::to_string);
    if let Some(method) = envelope.pointer("/cutex/method").and_then(Value::as_str) {
        let params = envelope.pointer("/cutex/params").cloned().unwrap_or(Value::Null);
        let kind = match method {
            STATE_CHANGED => "stateChanged",
            DELIVERY_CHANGED => "deliveryChanged",
            "cutex/stream/reset" => "streamReset",
            _ => "legacy",
        };
        let payload = if kind == "legacy" { json!({"method": method, "params": params}) } else { params };
        push(trace, source, kind, session.as_deref(), sequence, payload);
    } else if let Some(method) = envelope.pointer("/native/message/method").and_then(Value::as_str) {
        let params = envelope.pointer("/native/message/params").cloned().unwrap_or(Value::Null);
        let turn = params.pointer("/turn/id").or(params.get("turnId")).cloned().unwrap_or(Value::Null);
        let status = params.pointer("/turn/status").cloned().unwrap_or(Value::Null);
        // Item events name their item (S27 orders a read against tool items).
        let item = params.get("item");
        let mut payload = json!({"method": method, "turnId": turn, "status": status});
        if let Some(item) = item {
            payload["itemId"] = item["id"].clone();
            payload["itemType"] = item["type"].clone();
            payload["clientId"] = item.get("clientId").cloned().unwrap_or(Value::Null);
        }
        push(trace, source, "native", session.as_deref(), sequence, payload);
    }
}
