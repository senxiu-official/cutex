//! Management listeners: several bind addresses (loopback + Tailscale),
//! per-connection context (remote or loopback listener, peer address, the
//! client token that authenticated the request), keep-alive, a connection
//! cap and CORS.
//!
//! Remote (non-loopback) listeners accept only Management client / device
//! tokens; see `docs/research/cutex-mobile-api.md` in cutex-desktop.

use std::cell::RefCell;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;

use crate::management::client_tokens::VerifiedClientToken;

/// Concurrent connections per daemon (SSE streams included).
pub const MAX_CONNECTIONS: usize = 256;
/// Idle time between two requests on a kept-alive connection.
pub const KEEP_ALIVE_IDLE: Duration = Duration::from_secs(30);

pub const DEFAULT_CORS_ORIGINS: [&str; 3] = [
    "http://localhost",
    "https://localhost",
    "capacitor://localhost",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListenSpec {
    Socket(SocketAddr),
    /// `tailscale ip -4` on this port, resolved (and re-resolved) in the background.
    Tailscale {
        port: u16,
    },
}

impl ListenSpec {
    /// `"<ip>:<port>"`, `"<ip>"` (default port), `"[v6]:port"`, `"tailscale"`
    /// or `"tailscale:<port>"`.
    pub fn parse(spec: &str, default_port: u16) -> anyhow::Result<Self> {
        let spec = spec.trim();
        if spec.eq_ignore_ascii_case("tailscale") {
            return Ok(Self::Tailscale { port: default_port });
        }
        if let Some(port) = spec
            .strip_prefix("tailscale:")
            .or_else(|| spec.strip_prefix("TAILSCALE:"))
        {
            let port = port
                .parse::<u16>()
                .with_context(|| format!("invalid port in listen address {spec}"))?;
            return Ok(Self::Tailscale { port });
        }
        if let Ok(socket) = spec.parse::<SocketAddr>() {
            return Ok(Self::Socket(socket));
        }
        let ip = spec
            .parse::<IpAddr>()
            .with_context(|| format!("invalid management listen address: {spec}"))?;
        Ok(Self::Socket(SocketAddr::new(ip, default_port)))
    }

    pub fn port(&self) -> u16 {
        match self {
            Self::Socket(socket) => socket.port(),
            Self::Tailscale { port } => *port,
        }
    }

    pub fn is_loopback(&self) -> bool {
        matches!(self, Self::Socket(socket) if socket.ip().is_loopback())
    }
}

#[derive(Clone, Debug)]
pub struct ConnectionInfo {
    /// Accepted on a non-loopback listener.
    pub remote: bool,
    pub peer: SocketAddr,
}

thread_local! {
    static CONNECTION: RefCell<Option<ConnectionInfo>> = const { RefCell::new(None) };
    static CLIENT_TOKEN: RefCell<Option<VerifiedClientToken>> = const { RefCell::new(None) };
}

/// The connection the current thread serves (None outside a listener, e.g. tests).
pub fn current_connection() -> Option<ConnectionInfo> {
    CONNECTION.with(|cell| cell.borrow().clone())
}

pub fn set_current_connection(info: Option<ConnectionInfo>) {
    CONNECTION.with(|cell| *cell.borrow_mut() = info);
}

/// The client / device token that authenticated the current request.
pub fn current_client_token() -> Option<VerifiedClientToken> {
    CLIENT_TOKEN.with(|cell| cell.borrow().clone())
}

pub fn set_current_client_token(token: Option<VerifiedClientToken>) {
    CLIENT_TOKEN.with(|cell| *cell.borrow_mut() = token);
}

/// True unless the current request arrived on a remote listener.
pub fn current_request_is_loopback() -> bool {
    current_connection().is_none_or(|info| !info.remote)
}

static ACTIVE_REMOTE: Mutex<Vec<SocketAddr>> = Mutex::new(Vec::new());
static MAGIC_DNS_NAME: Mutex<Option<String>> = Mutex::new(None);

/// Remote listeners that are bound right now.
pub fn active_remote_listeners() -> Vec<SocketAddr> {
    ACTIVE_REMOTE
        .lock()
        .map(|active| active.clone())
        .unwrap_or_default()
}

/// `http://<ip>:<port>` per active remote listener, then the MagicDNS name
/// for each Tailscale listener port (when known).
pub fn pairing_urls() -> Vec<String> {
    let active = active_remote_listeners();
    let mut urls: Vec<String> = active
        .iter()
        .filter(|socket| !socket.ip().is_unspecified())
        .map(|socket| match socket.ip() {
            IpAddr::V4(ip) => format!("http://{ip}:{}", socket.port()),
            IpAddr::V6(ip) => format!("http://[{ip}]:{}", socket.port()),
        })
        .collect();
    let name = MAGIC_DNS_NAME.lock().ok().and_then(|name| name.clone());
    if let Some(name) = name {
        for socket in &active {
            if is_tailscale_ip(socket.ip()) {
                let url = format!("http://{name}:{}", socket.port());
                if !urls.contains(&url) {
                    urls.push(url);
                }
            }
        }
    }
    urls
}

fn is_tailscale_ip(ip: IpAddr) -> bool {
    // CGNAT range 100.64.0.0/10 used by Tailscale.
    matches!(ip, IpAddr::V4(v4) if v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
}

static CORS_ORIGINS: std::sync::RwLock<Option<Vec<String>>> = std::sync::RwLock::new(None);

/// Configure the CORS allow-list (daemon start, and again when
/// `/v2/settings` changes it). Unset = the defaults.
pub fn configure_cors_origins(origins: Option<Vec<String>>) {
    if let Ok(mut slot) = CORS_ORIGINS.write() {
        *slot = origins;
    }
}

pub fn cors_origin_allowed(origin: &str) -> bool {
    let configured = CORS_ORIGINS.read().ok().and_then(|slot| slot.clone());
    match configured {
        Some(list) => list.iter().any(|allowed| allowed == origin),
        None => DEFAULT_CORS_ORIGINS.contains(&origin),
    }
}

/// Validate a CORS allow-list: exact origins `scheme://host[:port]` (no path,
/// query or credentials), at most 20.
pub fn validate_cors_origins(origins: &[String]) -> Result<Vec<String>, String> {
    if origins.len() > 20 {
        return Err("at most 20 origins".into());
    }
    let mut out = Vec::new();
    for origin in origins {
        let trimmed = origin.trim().trim_end_matches('/');
        let url = url::Url::parse(trimmed).map_err(|_| format!("{origin}: not an origin (scheme://host[:port])"))?;
        if url.host_str().is_none_or(str::is_empty)
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || !matches!(url.path(), "" | "/")
        {
            return Err(format!("{origin}: not an origin (scheme://host[:port], no path)"));
        }
        if !out.iter().any(|known: &String| known == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}

static DEFAULT_LISTEN_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

/// The port entries without one use (`cutex management serve --port`).
pub fn set_default_listen_port(port: u16) {
    DEFAULT_LISTEN_PORT.store(port, Ordering::SeqCst);
}

pub fn default_listen_port() -> u16 {
    match DEFAULT_LISTEN_PORT.load(Ordering::SeqCst) {
        0 => crate::management::service::management_port(&crate::config::store::load_codez_config()),
        port => port,
    }
}

/// The listen list the daemon uses when `management_listen` is unset.
pub const DEFAULT_LISTEN: [&str; 1] = ["127.0.0.1"];

/// One validated `management_listen` entry (`/v2/settings` view).
pub fn listen_entry_kind(spec: &ListenSpec) -> &'static str {
    match spec {
        ListenSpec::Tailscale { .. } => "tailscale",
        ListenSpec::Socket(socket) if socket.ip().is_loopback() => "loopback",
        ListenSpec::Socket(socket) if socket.ip().is_unspecified() => "wildcard",
        ListenSpec::Socket(_) => "address",
    }
}

/// Validate a `management_listen` list (entries without a port use
/// `default_port`). Rules: 1–16 entries, each `tailscale[:port]` or an
/// IPv4 / IPv6 address with optional port, no duplicates, at least one
/// loopback or wildcard entry (the desktop and CLI connect over loopback), and
/// a wildcard (`0.0.0.0`, `::`) may not share its port with other entries
/// (the OS refuses the second bind). Returns the view rows
/// `{entry, kind, port, warning}`; wildcards carry a warning.
pub fn validate_listen_entries(entries: &[String], default_port: u16) -> Result<Vec<serde_json::Value>, String> {
    crate::config::instance::check_listen_entries(entries, default_port)?;
    if entries.is_empty() || entries.len() > 16 {
        return Err("managementListen: 1-16 entries (null = default loopback only)".into());
    }
    let mut specs: Vec<(String, ListenSpec)> = Vec::new();
    for entry in entries {
        let trimmed = entry.trim();
        let spec = ListenSpec::parse(trimmed, default_port).map_err(|error| format!("managementListen: {error:#}"))?;
        if spec.port() == 0 {
            return Err(format!("managementListen: {trimmed}: port 0 is not allowed"));
        }
        if specs.iter().any(|(_, known)| *known == spec) {
            return Err(format!("managementListen: duplicate entry {trimmed}"));
        }
        specs.push((trimmed.to_string(), spec));
    }
    if !specs
        .iter()
        .any(|(_, spec)| matches!(listen_entry_kind(spec), "loopback" | "wildcard"))
    {
        return Err("managementListen: keep a loopback entry (127.0.0.1 or ::1); the desktop and CLI connect over loopback".into());
    }
    for (entry, spec) in &specs {
        if listen_entry_kind(spec) == "wildcard" {
            if let Some((other, _)) = specs
                .iter()
                .find(|(other, o)| other != entry && o.port() == spec.port())
            {
                return Err(format!(
                    "managementListen: {entry} already listens on every address of port {}; remove {other} or use another port",
                    spec.port()
                ));
            }
        }
    }
    Ok(specs
        .iter()
        .map(|(entry, spec)| {
            let kind = listen_entry_kind(spec);
            serde_json::json!({
                "entry": entry,
                "kind": kind,
                "port": spec.port(),
                "warning": (kind == "wildcard").then_some(
                    "listens on every network interface (LAN, VPN and public networks); only client / device tokens are accepted remotely"
                ),
            })
        })
        .collect())
}

/// Response headers for an allowed `Origin` (empty string otherwise).
pub fn cors_headers(origin: Option<&str>) -> String {
    match origin.filter(|origin| cors_origin_allowed(origin)) {
        Some(origin) => format!(
            "Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\nAccess-Control-Expose-Headers: ETag, Retry-After\r\n"
        ),
        None => String::new(),
    }
}

pub fn cors_preflight_headers(origin: Option<&str>) -> String {
    let base = cors_headers(origin);
    if base.is_empty() {
        return base;
    }
    format!(
        "{base}Access-Control-Allow-Methods: GET, POST, PUT, PATCH, DELETE, OPTIONS\r\nAccess-Control-Allow-Headers: Authorization, Content-Type, If-Match, Last-Event-ID, Accept-Encoding\r\nAccess-Control-Max-Age: 600\r\n"
    )
}

struct ConnectionPool {
    open: AtomicUsize,
    bulk: AtomicUsize,
}
static CONNECTIONS: ConnectionPool = ConnectionPool {
    open: AtomicUsize::new(0),
    bulk: AtomicUsize::new(0),
};

/// Holds one connection slot; released on drop.
pub struct ConnectionSlot { bulk: bool, pool: &'static ConnectionPool }

impl ConnectionSlot {
    pub fn acquire() -> Option<Self> {
        Self::acquire_from(&CONNECTIONS)
    }

    fn acquire_from(pool: &'static ConnectionPool) -> Option<Self> {
        let previous = pool.open.fetch_add(1, Ordering::SeqCst);
        if previous >= MAX_CONNECTIONS {
            pool.open.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self { bulk: false, pool })
    }

    /// Bulk keep-alive connections retain their bulk slot while idle. This
    /// leaves capacity for new control connections, not just active requests.
    pub fn restrict_to_bulk(&mut self) -> bool {
        if self.bulk { return true; }
        let limit = MAX_CONNECTIONS - crate::management::v2::control_capacity::CONTROL_CONNECTION_RESERVE;
        if self.pool.bulk.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < limit).then_some(n + 1)).is_err() { return false; }
        self.bulk = true;
        true
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        if self.bulk { self.pool.bulk.fetch_sub(1, Ordering::AcqRel); }
        self.pool.open.fetch_sub(1, Ordering::SeqCst);
    }
}

pub fn open_connections() -> usize {
    CONNECTIONS.open.load(Ordering::SeqCst)
}

pub fn bulk_connections() -> usize {
    CONNECTIONS.bulk.load(Ordering::Relaxed)
}

/// Accept loop for one bound listener. `serve` handles one connection on its
/// own thread.
pub fn accept_loop(
    listener: TcpListener,
    remote: bool,
    serve: fn(TcpStream, ConnectionInfo, ConnectionSlot),
) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let peer = stream
                    .peer_addr()
                    .unwrap_or_else(|_| SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
                dispatch_connection(stream, peer, remote, serve);
            }
            Err(error) => {
                eprintln!("\x1b[33mwarning:\x1b[0m management API accept failed: {error}")
            }
        }
    }
}

fn dispatch_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    remote: bool,
    serve: fn(TcpStream, ConnectionInfo, ConnectionSlot),
) {
    let Some(slot) = ConnectionSlot::acquire() else {
        std::thread::spawn(move || {
            let _ = crate::management::v2::server::write_v2_error(
                &mut stream,
                503,
                "Service Unavailable",
                "too_many_connections",
                "the management API connection limit is reached",
                true,
                serde_json::json!({ "maxConnections": MAX_CONNECTIONS }),
            );
        });
        return;
    };
    // A wildcard listener also accepts loopback peers: those are local.
    let remote = remote && !peer.ip().is_loopback();
    let info = ConnectionInfo { remote, peer };
    std::thread::spawn(move || serve(stream, info, slot));
}

/// How often a stoppable listener checks its stop flag between accepts.
const ACCEPT_POLL: Duration = Duration::from_millis(100);

/// An accept loop that can be stopped (the Tailscale listener, re-bound when
/// the address changes). Stopping joins the thread and closes the socket;
/// connections already accepted keep running.
pub struct StoppableListener {
    socket: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

impl StoppableListener {
    pub fn spawn(
        listener: TcpListener,
        remote: bool,
        serve: fn(TcpStream, ConnectionInfo, ConnectionSlot),
    ) -> std::io::Result<Self> {
        let socket = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::Builder::new()
            .name(format!("cutex-listener-{socket}"))
            .spawn(move || {
                while !flag.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            // Accepted sockets may inherit non-blocking mode.
                            if stream.set_nonblocking(false).is_ok() {
                                dispatch_connection(stream, peer, remote, serve);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(ACCEPT_POLL)
                        }
                        Err(error) => {
                            eprintln!("\x1b[33mwarning:\x1b[0m management API accept failed on {socket}: {error}");
                            std::thread::sleep(ACCEPT_POLL);
                        }
                    }
                }
            })?;
        Ok(Self { socket, stop, handle })
    }

    pub fn socket(&self) -> SocketAddr {
        self.socket
    }

    /// Stop accepting, wait for the accept thread, release the socket.
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.handle.join();
    }
}

/// Re-bind a listener to `socket`: binds the new address first, then stops
/// the previous listener. Returns the old socket that was stopped, if any.
pub fn rebind(
    current: &mut Option<StoppableListener>,
    socket: SocketAddr,
    serve: fn(TcpStream, ConnectionInfo, ConnectionSlot),
) -> std::io::Result<Option<SocketAddr>> {
    if current.as_ref().is_some_and(|listener| listener.socket() == socket) {
        return Ok(None);
    }
    crate::config::instance::ensure_port_allowed(socket.port())?;
    let listener = StoppableListener::spawn(TcpListener::bind(socket)?, true, serve)?;
    let old = current.replace(listener);
    Ok(old.map(|old| {
        let socket = old.socket();
        old.stop();
        socket
    }))
}

/// Bind every spec. Loopback / explicit sockets bind now (an error is fatal
/// for loopback, logged for others); Tailscale specs bind in the background
/// as soon as `tailscale ip -4` answers, and re-bind when the address changes.
/// `allow_remote` false refuses every remote listener (no root credential).
pub fn start_listeners(
    specs: &[ListenSpec],
    allow_remote: bool,
    serve: fn(TcpStream, ConnectionInfo, ConnectionSlot),
) -> anyhow::Result<Vec<std::thread::JoinHandle<()>>> {
    let mut handles = Vec::new();
    for spec in specs {
        if !spec.is_loopback() && !allow_remote {
            eprintln!(
                "\x1b[33mwarning:\x1b[0m refusing management listener {spec:?}: remote listeners need a Management root credential (management_api_token) so client/device tokens can authenticate"
            );
            continue;
        }
        match spec {
            ListenSpec::Socket(socket) => {
                crate::config::instance::ensure_port_allowed(socket.port())?;
                let listener = match TcpListener::bind(socket) {
                    Ok(listener) => listener,
                    Err(error) if !socket.ip().is_loopback() => {
                        eprintln!(
                            "\x1b[33mwarning:\x1b[0m could not bind management listener {socket}: {error}"
                        );
                        continue;
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("Failed to bind cutex management API on {socket}")
                        })
                    }
                };
                let remote = !socket.ip().is_loopback();
                if remote {
                    if let Ok(mut active) = ACTIVE_REMOTE.lock() {
                        active.push(*socket);
                    }
                }
                println!(
                    "cutex management API listening on {}",
                    crate::management::service::management_health_url(socket.ip(), socket.port())
                );
                handles.push(std::thread::spawn(move || {
                    accept_loop(listener, remote, serve)
                }));
            }
            ListenSpec::Tailscale { port } => {
                let port = *port;
                handles.push(
                    std::thread::Builder::new()
                        .name("cutex-tailscale-listener".into())
                        .spawn(move || tailscale_listener_loop(port, serve))?,
                );
            }
        }
    }
    Ok(handles)
}

fn tailscale_listener_loop(port: u16, serve: fn(TcpStream, ConnectionInfo, ConnectionSlot)) {
    let mut backoff = Duration::from_secs(5);
    let mut bound: Option<SocketAddr> = None;
    let mut listener: Option<StoppableListener> = None;
    let mut last_problem: Option<String> = None;
    loop {
        match tailscale_ipv4() {
            Ok(ip) => {
                let socket = SocketAddr::new(IpAddr::V4(ip), port);
                if bound != Some(socket) {
                    // A changed address: bind the new one, then stop the old
                    // accept thread and close its socket.
                    match rebind(&mut listener, socket, serve) {
                        Ok(_) => {
                            if let Ok(mut active) = ACTIVE_REMOTE.lock() {
                                if let Some(old) = bound {
                                    active.retain(|addr| *addr != old);
                                }
                                active.push(socket);
                            }
                            if let Ok(name) = tailscale_dns_name() {
                                if let Ok(mut slot) = MAGIC_DNS_NAME.lock() {
                                    *slot = Some(name);
                                }
                            }
                            println!(
                                "cutex management API listening on {} (tailscale)",
                                crate::management::service::management_health_url(
                                    socket.ip(),
                                    port
                                )
                            );
                            bound = Some(socket);
                            last_problem = None;
                        }
                        Err(error) => {
                            let problem = format!("bind {socket}: {error}");
                            if last_problem.as_deref() != Some(problem.as_str()) {
                                eprintln!("\x1b[33mwarning:\x1b[0m tailscale management listener: {problem}");
                                last_problem = Some(problem);
                            }
                        }
                    }
                }
                backoff = Duration::from_secs(60);
            }
            Err(error) => {
                let problem = format!("{error:#}");
                if last_problem.as_deref() != Some(problem.as_str()) {
                    eprintln!(
                        "\x1b[33mwarning:\x1b[0m tailscale management listener waiting: {problem}"
                    );
                    last_problem = Some(problem);
                }
                backoff = if bound.is_some() {
                    Duration::from_secs(60)
                } else {
                    (backoff * 2).min(Duration::from_secs(60))
                };
            }
        }
        std::thread::sleep(backoff);
    }
}

fn tailscale_command() -> Command {
    #[cfg(windows)]
    {
        let installed = std::path::Path::new(r"C:\Program Files\Tailscale\tailscale.exe");
        if installed.exists() {
            return Command::new(installed);
        }
    }
    Command::new("tailscale")
}

fn tailscale_output(args: &[&str]) -> anyhow::Result<String> {
    let mut command = tailscale_command();
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command.output().context("run tailscale")?;
    if !output.status.success() {
        anyhow::bail!(
            "tailscale {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn tailscale_ipv4() -> anyhow::Result<Ipv4Addr> {
    let output = tailscale_output(&["ip", "-4"])?;
    output
        .lines()
        .find_map(|line| line.trim().parse::<Ipv4Addr>().ok())
        .ok_or_else(|| anyhow::anyhow!("tailscale ip -4 printed no IPv4 address"))
}

fn tailscale_dns_name() -> anyhow::Result<String> {
    let output = tailscale_output(&["status", "--json", "--self=true", "--peers=false"])?;
    let value: serde_json::Value = serde_json::from_str(&output)?;
    value
        .pointer("/Self/DNSName")
        .and_then(serde_json::Value::as_str)
        .map(|name| name.trim_end_matches('.').to_string())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no MagicDNS name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_keepalive_connections_cannot_take_reserved_control_slots() {
        let pool = Box::leak(Box::new(ConnectionPool {
            open: AtomicUsize::new(0), bulk: AtomicUsize::new(0),
        }));
        let reserve = crate::management::v2::control_capacity::CONTROL_CONNECTION_RESERVE;
        let bulk: Vec<_> = (0..MAX_CONNECTIONS - reserve).map(|_| {
            let mut slot = ConnectionSlot::acquire_from(pool).unwrap();
            assert!(slot.restrict_to_bulk());
            assert!(slot.restrict_to_bulk(), "keepalive reuse does not double charge");
            slot
        }).collect();
        let mut extra = ConnectionSlot::acquire_from(pool).unwrap();
        assert!(!extra.restrict_to_bulk(), "bulk cannot consume the control reserve");
        drop(extra);
        let controls: Vec<_> = (0..reserve).map(|_| ConnectionSlot::acquire_from(pool).unwrap()).collect();
        assert!(ConnectionSlot::acquire_from(pool).is_none());
        drop((bulk, controls));
        assert_eq!(pool.open.load(Ordering::SeqCst), 0);
        assert_eq!(pool.bulk.load(Ordering::SeqCst), 0);
    }

    fn answer_ok(mut stream: TcpStream, _info: ConnectionInfo, _slot: ConnectionSlot) {
        use std::io::Write;
        let _ = stream.write_all(b"ok");
    }

    fn read_answer(socket: SocketAddr) -> std::io::Result<String> {
        use std::io::Read;
        let mut stream = TcpStream::connect_timeout(&socket, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut text = String::new();
        stream.read_to_string(&mut text)?;
        Ok(text)
    }

    // macOS lo0 carries only 127.0.0.1 (127.0.0.2 needs an `ifconfig lo0 alias`).
    #[cfg_attr(target_os = "macos", ignore = "macOS loopback has no 127.0.0.2")]
    #[test]
    fn rebind_stops_the_old_listener_and_serves_the_new_address() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let first: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let second: SocketAddr = format!("127.0.0.2:{port}").parse().unwrap();
        let mut current = None;
        assert_eq!(rebind(&mut current, first, answer_ok).unwrap(), None);
        assert_eq!(read_answer(first).unwrap(), "ok");
        // Same address: nothing changes.
        assert_eq!(rebind(&mut current, first, answer_ok).unwrap(), None);
        assert_eq!(read_answer(first).unwrap(), "ok");
        // Address change: the new one serves, the old thread is gone and its
        // socket closed (the port is free again on the old address).
        assert_eq!(rebind(&mut current, second, answer_ok).unwrap(), Some(first));
        assert_eq!(read_answer(second).unwrap(), "ok");
        assert!(read_answer(first).is_err(), "old listener still accepts");
        drop(TcpListener::bind(first).expect("old socket released"));
        current.take().unwrap().stop();
        assert!(read_answer(second).is_err());
    }

    #[test]
    fn listen_specs_parse() {
        assert_eq!(
            ListenSpec::parse("127.0.0.1:24270", 1).unwrap(),
            ListenSpec::Socket("127.0.0.1:24270".parse().unwrap())
        );
        assert_eq!(
            ListenSpec::parse("100.64.0.12", 24270).unwrap(),
            ListenSpec::Socket("100.64.0.12:24270".parse().unwrap())
        );
        assert_eq!(
            ListenSpec::parse("tailscale", 24270).unwrap(),
            ListenSpec::Tailscale { port: 24270 }
        );
        assert_eq!(
            ListenSpec::parse("tailscale:24400", 24270).unwrap(),
            ListenSpec::Tailscale { port: 24400 }
        );
        assert!(ListenSpec::parse("nonsense", 24270).is_err());
        assert!(ListenSpec::parse("127.0.0.1", 24270).unwrap().is_loopback());
        assert!(!ListenSpec::parse("tailscale", 24270).unwrap().is_loopback());
        assert!(!ListenSpec::parse("0.0.0.0", 24270).unwrap().is_loopback());
    }

    #[test]
    fn cors_allow_list_is_exact() {
        assert!(cors_origin_allowed("http://localhost"));
        assert!(cors_origin_allowed("capacitor://localhost"));
        assert!(!cors_origin_allowed("http://localhost.evil.example"));
        assert!(!cors_origin_allowed("http://localhost:8080"));
        assert_eq!(cors_headers(Some("https://evil.example")), "");
        assert_eq!(cors_headers(None), "");
        let preflight = cors_preflight_headers(Some("https://localhost"));
        assert!(preflight.contains("Access-Control-Allow-Origin: https://localhost\r\n"));
        assert!(preflight.contains("Last-Event-ID"));
        assert!(preflight.contains("If-Match"));
    }

    #[test]
    fn listen_entries_validation() {
        let list = |entries: &[&str]| entries.iter().map(|e| e.to_string()).collect::<Vec<_>>();
        let rows = validate_listen_entries(&list(&["127.0.0.1:24270", "tailscale", "10.0.0.20:24271", "[::1]:24270"]), 24270).unwrap();
        let kinds: Vec<&str> = rows.iter().map(|r| r["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["loopback", "tailscale", "address", "loopback"]);
        assert!(rows.iter().all(|r| r["warning"].is_null()));
        let wildcard = validate_listen_entries(&list(&["0.0.0.0:24280"]), 24270).unwrap();
        assert_eq!(wildcard[0]["kind"], "wildcard");
        assert_eq!(wildcard[0]["port"], 24280);
        assert!(wildcard[0]["warning"].is_string());
        assert_eq!(validate_listen_entries(&list(&["::"]), 24270).unwrap()[0]["kind"], "wildcard");
        // Errors: no loopback, wildcard sharing its port, duplicates, junk, port 0, empty.
        assert!(validate_listen_entries(&list(&["tailscale"]), 24270).unwrap_err().contains("loopback"));
        assert!(validate_listen_entries(&list(&["0.0.0.0", "127.0.0.1"]), 24270).is_err());
        assert!(validate_listen_entries(&list(&["0.0.0.0", "tailscale"]), 24270).is_err());
        assert!(validate_listen_entries(&list(&["0.0.0.0", "tailscale:24271"]), 24270).is_ok());
        assert!(validate_listen_entries(&list(&["127.0.0.1", "127.0.0.1:24270"]), 24270).unwrap_err().contains("duplicate"));
        assert!(validate_listen_entries(&list(&["127.0.0.1", "lan"]), 24270).is_err());
        assert!(validate_listen_entries(&list(&["127.0.0.1:0"]), 24270).is_err());
        assert!(validate_listen_entries(&[], 24270).is_err());
    }

    #[test]
    fn cors_origin_validation() {
        let list = |entries: &[&str]| entries.iter().map(|e| e.to_string()).collect::<Vec<_>>();
        assert_eq!(
            validate_cors_origins(&list(&["capacitor://localhost", "http://10.0.0.5:5173/", "http://10.0.0.5:5173"])).unwrap(),
            ["capacitor://localhost", "http://10.0.0.5:5173"]
        );
        assert!(validate_cors_origins(&list(&["http://localhost/app"])).is_err());
        assert!(validate_cors_origins(&list(&["localhost"])).is_err());
        assert!(validate_cors_origins(&list(&["http://user:pw@localhost"])).is_err());
    }

    #[test]
    fn wildcard_listener_treats_loopback_peers_as_local() {
        fn record(_stream: TcpStream, info: ConnectionInfo, _slot: ConnectionSlot) {
            SEEN.lock().unwrap().push(info.remote);
        }
        static SEEN: Mutex<Vec<bool>> = Mutex::new(Vec::new());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = listener.local_addr().unwrap();
        let running = StoppableListener::spawn(listener, true, record).unwrap();
        drop(TcpStream::connect(socket).unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while SEEN.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        running.stop();
        assert_eq!(*SEEN.lock().unwrap(), [false]);
    }

    #[test]
    fn tailscale_range() {
        assert!(is_tailscale_ip("100.64.0.27".parse().unwrap()));
        assert!(!is_tailscale_ip(std::net::Ipv4Addr::new(100, 128, 0, 1).into())); // first address past 100.64.0.0/10
        assert!(!is_tailscale_ip("10.0.0.2".parse().unwrap()));
    }
}

/// Lets a long-lived stream notice that the client / device token that
/// opened it was revoked (in process: at once; by the CLI: within ~2 s).
pub struct RevocationWatch {
    stream: Option<TcpStream>,
    last_peer_check: Option<std::time::Instant>,
    token: Option<VerifiedClientToken>,
    generation: u64,
    last_check: std::time::Instant,
    validator: Box<dyn Fn(&VerifiedClientToken) -> bool + Send>,
}

/// How often a stream loop wakes up to check for revocation.
pub const STREAM_TICK: Duration = Duration::from_millis(500);
const REVOCATION_RECHECK: Duration = Duration::from_secs(2);

impl RevocationWatch {
    pub fn for_current_request() -> Self {
        Self::new(current_client_token())
    }

    /// Real HTTP streams attach their socket; in-memory writers use `new`.
    /// The clone never performs concurrent I/O with the response thread.
    pub fn for_stream(stream: &TcpStream) -> std::io::Result<Self> {
        let mut watch = Self::for_current_request();
        watch.stream = Some(stream.try_clone()?);
        Ok(watch)
    }

    pub fn disconnected(&mut self) -> std::io::Result<bool> {
        let Some(stream) = self.stream.as_ref() else { return Ok(false); };
        // Busy event streams already discover failures on writes. Do not add
        // socket syscalls per event/delta; idle loops still check every tick.
        if self.last_peer_check.is_some_and(|at| at.elapsed() < STREAM_TICK) {
            return Ok(false);
        }
        self.last_peer_check = Some(std::time::Instant::now());
        crate::http::server::stream_disconnected(stream)
    }

    pub fn new(token: Option<VerifiedClientToken>) -> Self {
        Self::with_validator(token, VerifiedClientToken::still_valid)
    }

    /// `validator` answers whether the token is still valid (tests use a
    /// private store).
    pub fn with_validator(
        token: Option<VerifiedClientToken>,
        validator: impl Fn(&VerifiedClientToken) -> bool + Send + 'static,
    ) -> Self {
        Self {
            stream: None,
            last_peer_check: None,
            token,
            generation: crate::management::client_tokens::revocation_generation(),
            last_check: std::time::Instant::now(),
            validator: Box::new(validator),
        }
    }

    /// `Some(tokenId)` once the stream's token is no longer valid.
    pub fn revoked(&mut self) -> Option<String> {
        let token = self.token.as_ref()?;
        let generation = crate::management::client_tokens::revocation_generation();
        if generation == self.generation && self.last_check.elapsed() < REVOCATION_RECHECK {
            return None;
        }
        self.generation = generation;
        self.last_check = std::time::Instant::now();
        (!(self.validator)(token)).then(|| token.token_id.clone())
    }
}

/// `event: cutex/device/revoked` (no `id:`); the caller closes the stream.
pub fn write_revoked_frame(
    stream: &mut impl std::io::Write,
    token_id: &str,
) -> std::io::Result<()> {
    // Still issued: the token's user or grant changed (P1 users). The client
    // reconnects and gets the new view; it must not forget the token.
    let still_issued = crate::management::client_tokens::ClientTokenStore::open_default()
        .and_then(|store| store.list())
        .is_ok_and(|tokens| tokens.iter().any(|token| token.token_id == token_id));
    if still_issued {
        let data = serde_json::json!({
            "tokenId": token_id,
            "changedAt": crate::management::instance::now_utc(),
        });
        write!(stream, "event: cutex/access/changed\ndata: {data}\n\n")?;
        return stream.flush();
    }
    let data = serde_json::json!({
        "tokenId": token_id,
        "revokedAt": crate::management::instance::now_utc(),
    });
    write!(stream, "event: cutex/device/revoked\ndata: {data}\n\n")?;
    stream.flush()
}
