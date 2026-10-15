//! Health states, the pure state computation and the TCP/HTTP probe.

use std::io::Read;
use std::io::Write;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::TcpStream;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;

use super::store::ServiceRuntime;

/// A supervised process that has not answered yet is `starting` for this long.
pub const STARTUP_GRACE: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(800);
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ServiceHealth {
    Reachable,
    Unreachable,
    Starting,
    Stopped,
    CrashLooped,
}

impl ServiceHealth {
    pub fn label(self) -> &'static str {
        match self {
            Self::Reachable => "reachable",
            Self::Unreachable => "unreachable",
            Self::Starting => "starting",
            Self::Stopped => "stopped",
            Self::CrashLooped => "crashLooped",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "reachable" => Some(Self::Reachable),
            "unreachable" => Some(Self::Unreachable),
            "starting" => Some(Self::Starting),
            "stopped" => Some(Self::Stopped),
            "crashLooped" => Some(Self::CrashLooped),
            _ => None,
        }
    }
}

/// Runtime facts persisted by the supervisor (`state.json`) so every reader
/// (routes, Agent Bus MCP list) sees the same health.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSnapshot {
    #[serde(default)]
    pub last_traffic_at: Option<String>,
    pub health: ServiceHealth,
    #[serde(default)]
    pub last_seen_at: Option<String>,
    #[serde(default)]
    pub last_probe_at: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub restarts: u32,
    #[serde(default)]
    pub last_exit_code: Option<i32>,
    #[serde(default)]
    pub last_exit_at: Option<String>,
    #[serde(default)]
    pub next_restart_at: Option<String>,
}

/// What the supervisor knows about a service's process when it computes health.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessFacts {
    /// No process and none will be started automatically.
    Down,
    /// No process now, but the supervisor will restart it (crash backoff).
    RestartPending,
    Running {
        since_spawn: Duration,
        reached_since_spawn: bool,
    },
}

/// Health from the runtime kind, the process facts and the latest probe.
pub fn compute_health(
    runtime: ServiceRuntime,
    process: ProcessFacts,
    probe_ok: bool,
) -> ServiceHealth {
    if !runtime.supervised() {
        return if probe_ok {
            ServiceHealth::Reachable
        } else {
            ServiceHealth::Unreachable
        };
    }
    match process {
        ProcessFacts::Down => ServiceHealth::Stopped,
        ProcessFacts::RestartPending => ServiceHealth::Unreachable,
        ProcessFacts::Running { .. } if probe_ok => ServiceHealth::Reachable,
        ProcessFacts::Running {
            since_spawn,
            reached_since_spawn,
        } if !reached_since_spawn && since_spawn < STARTUP_GRACE => ServiceHealth::Starting,
        ProcessFacts::Running { .. } => ServiceHealth::Unreachable,
    }
}

fn connect(port: u16) -> Option<TcpStream> {
    [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ]
    .iter()
    .find_map(|address| TcpStream::connect_timeout(address, CONNECT_TIMEOUT).ok())
}

/// TCP connect to loopback; with a `path`, also `GET path` and require an
/// HTTP status below 500 (a 404 still proves the server is up).
pub fn probe(port: u16, path: Option<&str>) -> bool {
    let Some(mut stream) = connect(port) else {
        return false;
    };
    let Some(path) = path else {
        return true;
    };
    stream.set_read_timeout(Some(HTTP_TIMEOUT)).ok();
    stream.set_write_timeout(Some(HTTP_TIMEOUT)).ok();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUser-Agent: cutex-services-probe\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut buffer = [0_u8; 64];
    let mut read = 0;
    while read < 12 {
        match stream.read(&mut buffer[read..]) {
            Ok(0) | Err(_) => break,
            Ok(count) => read += count,
        }
    }
    http_status_ok(&buffer[..read])
}

fn http_status_ok(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(head);
    let mut parts = text.split_whitespace();
    matches!(parts.next(), Some(version) if version.starts_with("HTTP/"))
        && parts
            .next()
            .and_then(|code| code.parse::<u16>().ok())
            .is_some_and(|code| (100..500).contains(&code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn external_health_follows_probe() {
        assert_eq!(
            compute_health(ServiceRuntime::External, ProcessFacts::Down, true),
            ServiceHealth::Reachable
        );
        assert_eq!(
            compute_health(ServiceRuntime::External, ProcessFacts::Down, false),
            ServiceHealth::Unreachable
        );
    }

    #[test]
    fn supervised_health_transitions() {
        let fresh = ProcessFacts::Running {
            since_spawn: Duration::from_secs(1),
            reached_since_spawn: false,
        };
        let old = ProcessFacts::Running {
            since_spawn: STARTUP_GRACE + Duration::from_secs(1),
            reached_since_spawn: false,
        };
        let was_up = ProcessFacts::Running {
            since_spawn: Duration::from_secs(5),
            reached_since_spawn: true,
        };
        for runtime in [ServiceRuntime::Managed] {
            assert_eq!(
                compute_health(runtime, ProcessFacts::Down, true),
                ServiceHealth::Stopped
            );
            assert_eq!(
                compute_health(runtime, ProcessFacts::RestartPending, false),
                ServiceHealth::Unreachable
            );
            assert_eq!(
                compute_health(runtime, fresh, false),
                ServiceHealth::Starting
            );
            assert_eq!(
                compute_health(runtime, fresh, true),
                ServiceHealth::Reachable
            );
            assert_eq!(
                compute_health(runtime, old, false),
                ServiceHealth::Unreachable
            );
            assert_eq!(
                compute_health(runtime, was_up, false),
                ServiceHealth::Unreachable
            );
        }
    }

    #[test]
    fn http_status_parsing() {
        assert!(http_status_ok(b"HTTP/1.1 200 OK\r\n"));
        assert!(http_status_ok(b"HTTP/1.0 404 Not Found"));
        assert!(!http_status_ok(b"HTTP/1.1 503 Service Unavailable"));
        assert!(!http_status_ok(b"SSH-2.0-OpenSSH"));
        assert!(!http_status_ok(b""));
    }

    #[test]
    fn probe_tcp_and_http() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(probe(port, None));
        let server = std::thread::spawn(move || {
            // The TCP-only probe connection above is accepted first.
            let _ = listener.accept();
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 512];
            let _ = stream.read(&mut buffer);
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        assert!(probe(port, Some("/health")));
        server.join().unwrap();
        // A released ephemeral port can be re-bound at once by a parallel test,
        // so try a few fresh ports; a probe that always answers true still fails.
        let closed_port_refused = (0..5).any(|_| {
            let closed = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let closed_port = closed.local_addr().unwrap().port();
            drop(closed);
            !probe(closed_port, None)
        });
        assert!(closed_port_refused);
    }
}
