use std::collections::HashMap;
use std::fmt;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::net::TcpStream;
use std::net::ToSocketAddrs;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::mpsc::SyncSender;
use std::sync::mpsc::TryRecvError;
use std::sync::mpsc::TrySendError;
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

#[cfg(unix)]
use std::os::unix::net::UnixStream;

use serde_json::json;
use serde_json::Value;
use tungstenite::client::client_with_config;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::header::AUTHORIZATION;
use tungstenite::http::HeaderValue;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::Message;
use tungstenite::WebSocket;
use url::Host;
use url::Url;

use super::protocol::classify_inbound;
use super::protocol::error_response_message;
use super::protocol::notification_message;
use super::protocol::request_message;
use super::protocol::success_response_message;
use super::protocol::InboundMessage;
use super::protocol::RpcError;
use super::protocol::RpcNotification;
use super::protocol::RpcResponse;
use super::protocol::RpcResponseOutcome;
use super::protocol::RpcServerRequest;

const ACTOR_POLL_INTERVAL: Duration = Duration::from_millis(50);
const ACTIVE_SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(5);
const IDLE_SOCKET_PROBE_TIMEOUT: Duration = Duration::from_millis(1);
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_COMMAND_CAPACITY: usize = 128;
const DEFAULT_EVENT_CAPACITY: usize = 512;
const MAX_COMMANDS_PER_TICK: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppServerEndpoint {
    #[cfg(unix)]
    UnixSocket { socket_path: PathBuf },
    LoopbackWebSocket {
        url: String,
        bearer_token: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct AppServerClientOptions {
    pub endpoint: AppServerEndpoint,
    pub client_name: String,
    pub client_title: String,
    pub client_version: String,
    pub experimental_api: bool,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub command_capacity: usize,
    pub event_capacity: usize,
    /// Additional `initialize` capabilities (e.g. `claudeInteractiveApprovals`
    /// for the Claude adapter). The two standard keys always win.
    pub extra_capabilities: serde_json::Map<String, Value>,
}

impl AppServerClientOptions {
    pub fn new(endpoint: AppServerEndpoint) -> Self {
        Self {
            endpoint,
            client_name: "cutex".to_string(),
            client_title: "cutex".to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            experimental_api: true,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            command_capacity: DEFAULT_COMMAND_CAPACITY,
            event_capacity: DEFAULT_EVENT_CAPACITY,
            extra_capabilities: serde_json::Map::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppServerClientError {
    InvalidEndpoint(String),
    InvalidOptions(String),
    Connect(String),
    Transport(String),
    Protocol(String),
    Rpc(RpcError),
    Timeout { method: String },
    Disconnected(String),
    RequestIdInUse(String),
    Backpressure(&'static str),
    Shutdown,
}

impl fmt::Display for AppServerClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndpoint(message)
            | Self::InvalidOptions(message)
            | Self::Connect(message)
            | Self::Transport(message)
            | Self::Protocol(message)
            | Self::Disconnected(message)
            | Self::RequestIdInUse(message) => formatter.write_str(message),
            Self::Rpc(error) => write!(
                formatter,
                "app-server request failed with {}: {}",
                error.code, error.message
            ),
            Self::Timeout { method } => {
                write!(formatter, "timed out waiting for app-server {method}")
            }
            Self::Backpressure(queue) => write!(formatter, "app-server {queue} queue is full"),
            Self::Shutdown => formatter.write_str("app-server client is shut down"),
        }
    }
}

impl std::error::Error for AppServerClientError {}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RpcRequestIdKey {
    String(String),
    Integer(i64),
}

impl RpcRequestIdKey {
    fn from_value(id: &Value) -> Result<Self, AppServerClientError> {
        if let Some(id) = id.as_str() {
            return Ok(Self::String(id.to_string()));
        }
        if let Some(id) = id.as_i64() {
            return Ok(Self::Integer(id));
        }
        Err(AppServerClientError::Protocol(
            "app-server request id must be a string or signed 64-bit integer".to_string(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppServerEvent {
    Notification(RpcNotification),
    ServerRequest(RpcServerRequest),
    ProtocolViolation { message: String },
    Disconnected { reason: String },
}

#[derive(Clone)]
pub struct AppServerHandle {
    command_tx: SyncSender<ActorCommand>,
    next_id: Arc<AtomicU64>,
    default_timeout: Duration,
}

impl AppServerHandle {
    /// Clones of one actor share this identity; a reconnect in the same runtime
    /// generation is still a different connection.
    pub fn same_connection(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.next_id, &other.next_id)
    }

    /// Observe the exact response before later notifications are delivered.
    /// The observer survives a caller timeout (until response/disconnect), for
    /// ephemeral resources that must be cleaned up even after an unknown result.
    /// It must not block on native I/O; schedule follow-up work off the actor.
    pub fn request_observed_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        observer: Box<dyn FnOnce(&RpcResponse) + Send>,
    ) -> Result<Value, AppServerClientError> {
        let id = format!("cutex-observed-{}", uuid::Uuid::new_v4());
        let response = self.request_rpc_message_observed(
            request_message(json!(id), method, params),
            timeout,
            Some(observer),
        )?;
        match response.outcome {
            RpcResponseOutcome::Result(result) => Ok(result),
            RpcResponseOutcome::Error(error) => Err(AppServerClientError::Rpc(error)),
        }
    }
    pub fn request(&self, method: &str, params: Value) -> Result<Value, AppServerClientError> {
        self.request_with_timeout(method, params, self.default_timeout)
    }

    pub fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, AppServerClientError> {
        let id = self
            .next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
                (id <= i64::MAX as u64).then(|| id + 1)
            })
            .map_err(|_| {
                AppServerClientError::InvalidOptions(
                    "app-server request id space is exhausted".to_string(),
                )
            })?;
        let response = self.request_rpc_with_timeout(Value::from(id), method, params, timeout)?;
        match response.outcome {
            RpcResponseOutcome::Result(result) => Ok(result),
            RpcResponseOutcome::Error(error) => Err(AppServerClientError::Rpc(error)),
        }
    }

    pub fn request_raw(
        &self,
        id: Value,
        method: &str,
        params: Value,
    ) -> Result<Value, AppServerClientError> {
        self.request_raw_with_timeout(id, method, params, self.default_timeout)
    }

    pub fn request_raw_with_timeout(
        &self,
        id: Value,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, AppServerClientError> {
        self.request_rpc_message_with_timeout(request_message(id, method, params), timeout)
            .map(|response| response.raw)
    }

    pub fn request_raw_message(&self, message: Value) -> Result<Value, AppServerClientError> {
        self.request_raw_message_with_timeout(message, self.default_timeout)
    }

    pub fn request_raw_message_with_timeout(
        &self,
        message: Value,
        timeout: Duration,
    ) -> Result<Value, AppServerClientError> {
        self.request_rpc_message_with_timeout(message, timeout)
            .map(|response| response.raw)
    }

    fn request_rpc_with_timeout(
        &self,
        id: Value,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<RpcResponse, AppServerClientError> {
        self.request_rpc_message_with_timeout(request_message(id, method, params), timeout)
    }

    fn request_rpc_message_with_timeout(
        &self,
        message: Value,
        timeout: Duration,
    ) -> Result<RpcResponse, AppServerClientError> {
        self.request_rpc_message_observed(message, timeout, None)
    }

    fn request_rpc_message_observed(
        &self,
        message: Value,
        timeout: Duration,
        response_observer: Option<Box<dyn FnOnce(&RpcResponse) + Send>>,
    ) -> Result<RpcResponse, AppServerClientError> {
        let (id, method) = request_identity(&message)?;
        if method.is_empty() {
            return Err(AppServerClientError::Protocol(
                "app-server request method must not be empty".to_string(),
            ));
        }
        if timeout.is_zero() {
            return Err(AppServerClientError::InvalidOptions(
                "app-server request timeout must be positive".to_string(),
            ));
        }
        RpcRequestIdKey::from_value(&id)?;
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            AppServerClientError::InvalidOptions(
                "app-server request timeout exceeds the supported range".to_string(),
            )
        })?;
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        self.send_command(ActorCommand::Request {
            id,
            method: method.clone(),
            message,
            deadline,
            response_tx,
            response_observer,
        })?;
        response_rx
            .recv_timeout(timeout + ACTOR_POLL_INTERVAL + ACTOR_POLL_INTERVAL)
            .map_err(|error| match error {
                RecvTimeoutError::Timeout => AppServerClientError::Timeout {
                    method: method.clone(),
                },
                RecvTimeoutError::Disconnected => AppServerClientError::Disconnected(
                    "app-server actor stopped before returning a response".to_string(),
                ),
            })?
    }

    pub fn notify(&self, method: &str, params: Option<Value>) -> Result<(), AppServerClientError> {
        if method.is_empty() {
            return Err(AppServerClientError::Protocol(
                "app-server notification method must not be empty".to_string(),
            ));
        }
        self.send_command(ActorCommand::Send(notification_message(method, params)))
    }

    pub fn respond_result(&self, id: Value, result: Value) -> Result<(), AppServerClientError> {
        self.send_command(ActorCommand::Send(success_response_message(id, result)))
    }

    pub fn respond_error(
        &self,
        id: Value,
        code: i64,
        message: &str,
        data: Option<Value>,
    ) -> Result<(), AppServerClientError> {
        self.send_command(ActorCommand::Send(error_response_message(
            id, code, message, data,
        )))
    }

    pub fn respond_raw(&self, message: Value) -> Result<(), AppServerClientError> {
        match classify_inbound(message.clone()) {
            Ok(InboundMessage::Response(_)) => {}
            Ok(_) => {
                return Err(AppServerClientError::Protocol(
                    "app-server server-request response must be a JSON-RPC response".to_string(),
                ));
            }
            Err(error) => return Err(AppServerClientError::Protocol(error.to_string())),
        }
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        self.send_command(ActorCommand::Write {
            message,
            response_tx,
        })?;
        response_rx
            .recv_timeout(self.default_timeout + ACTOR_POLL_INTERVAL + ACTOR_POLL_INTERVAL)
            .map_err(|error| match error {
                RecvTimeoutError::Timeout => AppServerClientError::Timeout {
                    method: "native server-request response write".to_string(),
                },
                RecvTimeoutError::Disconnected => AppServerClientError::Disconnected(
                    "app-server actor stopped before confirming the response write".to_string(),
                ),
            })?
    }

    pub fn shutdown(&self) -> Result<(), AppServerClientError> {
        self.command_tx
            .send(ActorCommand::Shutdown)
            .map_err(|_| AppServerClientError::Shutdown)
    }

    fn send_command(&self, command: ActorCommand) -> Result<(), AppServerClientError> {
        self.command_tx
            .try_send(command)
            .map_err(|error| match error {
                TrySendError::Full(_) => AppServerClientError::Backpressure("command"),
                TrySendError::Disconnected(_) => AppServerClientError::Shutdown,
            })
    }
}

fn request_identity(message: &Value) -> Result<(Value, String), AppServerClientError> {
    let object = message.as_object().ok_or_else(|| {
        AppServerClientError::Protocol("app-server request must be a JSON object".to_string())
    })?;
    let id = object
        .get("id")
        .filter(|id| !id.is_null())
        .cloned()
        .ok_or_else(|| {
            AppServerClientError::Protocol("app-server request omitted id".to_string())
        })?;
    RpcRequestIdKey::from_value(&id)?;
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .filter(|method| !method.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            AppServerClientError::Protocol(
                "app-server request method must be a non-empty string".to_string(),
            )
        })?;
    Ok((id, method))
}

pub struct AppServerClient {
    handle: AppServerHandle,
    event_rx: Option<Receiver<AppServerEvent>>,
    actor: Option<JoinHandle<()>>,
    initialize_response: Value,
}

impl AppServerClient {
    pub fn connect(options: AppServerClientOptions) -> Result<Self, AppServerClientError> {
        if options.command_capacity == 0 || options.event_capacity == 0 {
            return Err(AppServerClientError::InvalidOptions(
                "app-server channel capacities must be positive".to_string(),
            ));
        }
        if options.connect_timeout.is_zero() || options.request_timeout.is_zero() {
            return Err(AppServerClientError::InvalidOptions(
                "app-server connect and request timeouts must be positive".to_string(),
            ));
        }
        let socket = connect_websocket(&options.endpoint, options.connect_timeout)?;
        let (command_tx, command_rx) = mpsc::sync_channel(options.command_capacity);
        let (event_tx, event_rx) = mpsc::sync_channel(options.event_capacity);
        let actor = thread::spawn(move || run_actor(socket, command_rx, event_tx));
        let handle = AppServerHandle {
            command_tx,
            next_id: Arc::new(AtomicU64::new(1)),
            default_timeout: options.request_timeout,
        };
        let mut capabilities = options.extra_capabilities.clone();
        capabilities.insert(
            "experimentalApi".to_string(),
            Value::Bool(options.experimental_api),
        );
        capabilities.insert("optOutNotificationMethods".to_string(), json!([]));
        let initialize_response = match handle.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": options.client_name,
                    "title": options.client_title,
                    "version": options.client_version,
                },
                "capabilities": capabilities,
            }),
        ) {
            Ok(response) => response,
            Err(error) => {
                drop(event_rx);
                let _ = handle.shutdown();
                let _ = actor.join();
                return Err(error);
            }
        };
        if let Err(error) = handle.notify("initialized", None) {
            drop(event_rx);
            let _ = handle.shutdown();
            let _ = actor.join();
            return Err(error);
        }
        Ok(Self {
            handle,
            event_rx: Some(event_rx),
            actor: Some(actor),
            initialize_response,
        })
    }

    pub fn handle(&self) -> AppServerHandle {
        self.handle.clone()
    }

    pub fn initialize_response(&self) -> &Value {
        &self.initialize_response
    }

    fn event_receiver(&self) -> Result<&Receiver<AppServerEvent>, AppServerClientError> {
        self.event_rx.as_ref().ok_or_else(|| {
            AppServerClientError::Disconnected("app-server event stream closed".to_string())
        })
    }

    pub fn recv_event(&self) -> Result<AppServerEvent, AppServerClientError> {
        self.event_receiver()?.recv().map_err(|_| {
            AppServerClientError::Disconnected("app-server event stream closed".to_string())
        })
    }

    pub fn recv_event_timeout(
        &self,
        timeout: Duration,
    ) -> Result<Option<AppServerEvent>, AppServerClientError> {
        match self.event_receiver()?.recv_timeout(timeout) {
            Ok(event) => Ok(Some(event)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(AppServerClientError::Disconnected(
                "app-server event stream closed".to_string(),
            )),
        }
    }
}

impl Drop for AppServerClient {
    fn drop(&mut self) {
        #[cfg(feature = "stock-launch-test-hook")]
        super::private_restart_phase("client.drop.begin");
        // The actor may be blocked applying lossless event backpressure. Drop
        // its receiver before joining so shutdown can always unblock the send.
        drop(self.event_rx.take());
        let _ = self.handle.shutdown();
        #[cfg(feature = "stock-launch-test-hook")]
        super::private_restart_phase("client.drop.actor_join.begin");
        if let Some(actor) = self.actor.take() {
            let _ = actor.join();
        }
        #[cfg(feature = "stock-launch-test-hook")]
        super::private_restart_phase("client.drop.end");
    }
}

#[cfg(test)]
pub(crate) fn saturated_event_client_for_test() -> (AppServerClient, Receiver<()>) {
    let (command_tx, command_rx) = mpsc::sync_channel(1);
    let handle = AppServerHandle {
        command_tx,
        next_id: Arc::new(AtomicU64::new(1)),
        default_timeout: Duration::from_secs(1),
    };
    let (event_tx, event_rx) = mpsc::sync_channel(1);
    event_tx
        .send(AppServerEvent::ProtocolViolation {
            message: "queued event".to_string(),
        })
        .expect("fill event queue");
    let (send_attempted_tx, send_attempted_rx) = mpsc::sync_channel(1);
    let actor = thread::spawn(move || {
        let _command_rx = command_rx;
        send_attempted_tx
            .send(())
            .expect("signal saturated send attempt");
        assert!(!emit_event(
            &event_tx,
            AppServerEvent::ProtocolViolation {
                message: "blocked event".to_string(),
            },
        ));
    });
    (
        AppServerClient {
            handle,
            event_rx: Some(event_rx),
            actor: Some(actor),
            initialize_response: json!({}),
        },
        send_attempted_rx,
    )
}

enum ActorCommand {
    Request {
        id: Value,
        method: String,
        message: Value,
        deadline: Instant,
        response_tx: SyncSender<Result<RpcResponse, AppServerClientError>>,
        response_observer: Option<Box<dyn FnOnce(&RpcResponse) + Send>>,
    },
    Send(Value),
    Write {
        message: Value,
        response_tx: SyncSender<Result<(), AppServerClientError>>,
    },
    Shutdown,
}

struct PendingRequest {
    method: String,
    deadline: Instant,
    response_tx: SyncSender<Result<RpcResponse, AppServerClientError>>,
    response_observer: Option<Box<dyn FnOnce(&RpcResponse) + Send>>,
    timed_out: bool,
}

enum ConnectionStream {
    #[cfg(unix)]
    Unix(UnixStream),
    Tcp {
        stream: TcpStream,
        #[cfg(windows)]
        read_timeout: std::cell::Cell<Option<Duration>>,
        #[cfg(windows)]
        handshake_deadline: std::cell::Cell<Option<Instant>>,
    },
}

impl ConnectionStream {
    fn tcp(stream: TcpStream) -> Self {
        Self::Tcp {
            stream,
            #[cfg(windows)]
            read_timeout: std::cell::Cell::new(None),
            #[cfg(windows)]
            handshake_deadline: std::cell::Cell::new(None),
        }
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.set_read_timeout(timeout),
            #[cfg(not(windows))]
            Self::Tcp { stream } => stream.set_read_timeout(timeout),
            #[cfg(windows)]
            Self::Tcp { stream, read_timeout, .. } => {
                if let Some(timeout) = timeout {
                    if timeout.is_zero() {
                        return Err(io::Error::new(io::ErrorKind::InvalidInput, "zero read timeout"));
                    }
                    windows_select_timeout(timeout)?;
                }
                // SO_RCVTIMEO expiry leaves a Winsock connection in an
                // indeterminate state. Keep recv blocking and poll readiness
                // instead; writes retain their independent blocking timeout.
                stream.set_read_timeout(None)?;
                read_timeout.set(timeout);
                Ok(())
            }
        }
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.set_write_timeout(timeout),
            Self::Tcp { stream, .. } => stream.set_write_timeout(timeout),
        }
    }
}

impl Read for ConnectionStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(buffer),
            #[cfg(not(windows))]
            Self::Tcp { stream } => stream.read(buffer),
            #[cfg(windows)]
            Self::Tcp { stream, read_timeout, handshake_deadline } => {
                if buffer.is_empty() {
                    return Ok(0);
                }
                let timeout = match handshake_deadline.get() {
                    Some(deadline) => {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            return Err(io::Error::new(io::ErrorKind::TimedOut, "websocket handshake deadline"));
                        }
                        Some(read_timeout.get().map_or(remaining, |timeout| timeout.min(remaining)))
                    }
                    None => read_timeout.get(),
                };
                windows_wait_readable(stream, timeout)?;
                // This actor owns the only reader: readiness cannot be
                // consumed by another thread before this blocking recv.
                stream.read(buffer)
            }
        }
    }
}

#[cfg(windows)]
fn windows_select_timeout(timeout: Duration) -> io::Result<windows_sys::Win32::Networking::WinSock::TIMEVAL> {
    // Round up to avoid turning a positive sub-microsecond timeout into a
    // busy poll. TIMEVAL uses signed 32-bit seconds even on 64-bit Windows.
    let micros = (u64::from(timeout.subsec_nanos()) + 999) / 1_000;
    let seconds = timeout.as_secs().checked_add(micros / 1_000_000)
        .and_then(|seconds| i32::try_from(seconds).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "read timeout exceeds Winsock TIMEVAL"))?;
    Ok(windows_sys::Win32::Networking::WinSock::TIMEVAL {
        tv_sec: seconds,
        tv_usec: (micros % 1_000_000) as i32,
    })
}

#[cfg(windows)]
fn windows_wait_readable(stream: &TcpStream, timeout: Option<Duration>) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{select, WSAGetLastError, FD_SET, SOCKET_ERROR};
    let mut readers = FD_SET { fd_count: 1, fd_array: [0; 64] };
    readers.fd_array[0] = stream.as_raw_socket() as _;
    let timeout = timeout.map(windows_select_timeout).transpose()?;
    // The set and optional TIMEVAL stay alive for the synchronous call.
    // Winsock ignores nfds; only this single owned socket is registered.
    let result = unsafe {
        select(0, &mut readers, std::ptr::null_mut(), std::ptr::null_mut(),
            timeout.as_ref().map_or(std::ptr::null(), |timeout| timeout))
    };
    if result == SOCKET_ERROR {
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    if result == 0 {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, "socket has no readable data"));
    }
    // EOF and reset are also readable; recv reports them normally. Never
    // hide a genuine transport failure such as 997 or 10054.
    Ok(())
}

impl Write for ConnectionStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(buffer),
            Self::Tcp { stream, .. } => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
            Self::Tcp { stream, .. } => stream.flush(),
        }
    }
}

fn connect_websocket(
    endpoint: &AppServerEndpoint,
    timeout: Duration,
) -> Result<WebSocket<ConnectionStream>, AppServerClientError> {
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        AppServerClientError::InvalidOptions(
            "app-server connect timeout exceeds the supported range".to_string(),
        )
    })?;
    let mut last_error = None;
    let (stream, request_url, bearer_token) = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(last_error.unwrap_or_else(|| {
                AppServerClientError::Connect(
                    "timed out connecting to the app-server endpoint".to_string(),
                )
            }));
        }
        match connect_stream(endpoint, remaining) {
            Ok(connection) => break connection,
            Err(error) => {
                last_error = Some(error);
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    continue;
                }
                thread::sleep(CONNECT_RETRY_INTERVAL.min(remaining));
            }
        }
    };
    let handshake_timeout = deadline.saturating_duration_since(Instant::now());
    if handshake_timeout.is_zero() {
        return Err(AppServerClientError::Connect(
            "timed out before the app-server websocket handshake".to_string(),
        ));
    }
    #[cfg(windows)]
    {
        let ConnectionStream::Tcp { handshake_deadline, .. } = &stream;
        handshake_deadline.set(Some(deadline));
    }
    stream
        .set_read_timeout(Some(handshake_timeout))
        .map_err(|error| AppServerClientError::Connect(error.to_string()))?;
    stream
        .set_write_timeout(Some(handshake_timeout))
        .map_err(|error| AppServerClientError::Connect(error.to_string()))?;
    let mut request = request_url
        .into_client_request()
        .map_err(|error| AppServerClientError::InvalidEndpoint(error.to_string()))?;
    if let Some(token) = bearer_token {
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|error| AppServerClientError::InvalidEndpoint(error.to_string()))?;
        request.headers_mut().insert(AUTHORIZATION, value);
    }
    let (socket, response) =
        client_with_config(request, stream, Some(app_server_websocket_config()))
            .map_err(|error| AppServerClientError::Connect(error.to_string()))?;
    if response.status() != 101 {
        return Err(AppServerClientError::Connect(format!(
            "app-server websocket upgrade returned {}",
            response.status()
        )));
    }
    #[cfg(windows)]
    {
        let ConnectionStream::Tcp { handshake_deadline, .. } = socket.get_ref();
        handshake_deadline.set(None);
    }
    socket
        .get_ref()
        .set_read_timeout(Some(IDLE_SOCKET_PROBE_TIMEOUT))
        .map_err(|error| AppServerClientError::Connect(error.to_string()))?;
    Ok(socket)
}

fn app_server_websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_frame_size(None)
        .max_message_size(None)
}

fn connect_stream(
    endpoint: &AppServerEndpoint,
    timeout: Duration,
) -> Result<(ConnectionStream, String, Option<String>), AppServerClientError> {
    match endpoint {
        #[cfg(unix)]
        AppServerEndpoint::UnixSocket { socket_path } => UnixStream::connect(socket_path)
            .map(|stream| {
                (
                    ConnectionStream::Unix(stream),
                    "ws://localhost/".to_string(),
                    None,
                )
            })
            .map_err(|error| {
                AppServerClientError::Connect(format!(
                    "failed to connect app-server Unix socket {}: {error}",
                    socket_path.display()
                ))
            }),
        AppServerEndpoint::LoopbackWebSocket { url, bearer_token } => {
            let addresses = loopback_websocket_addresses(url)?;
            let mut last_error = None;
            for address in addresses {
                match TcpStream::connect_timeout(&address, timeout) {
                    Ok(stream) => {
                        return Ok((
                            ConnectionStream::tcp(stream),
                            url.clone(),
                            bearer_token.clone(),
                        ));
                    }
                    Err(error) => last_error = Some(error),
                }
            }
            Err(AppServerClientError::Connect(format!(
                "failed to connect app-server loopback websocket {url}: {}",
                last_error
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "no loopback addresses resolved".to_string())
            )))
        }
    }
}

fn loopback_websocket_addresses(url: &str) -> Result<Vec<SocketAddr>, AppServerClientError> {
    let parsed = Url::parse(url)
        .map_err(|error| AppServerClientError::InvalidEndpoint(error.to_string()))?;
    if parsed.scheme() != "ws"
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(AppServerClientError::InvalidEndpoint(
            "app-server endpoint must be a loopback ws://host:port URL".to_string(),
        ));
    }
    let port = parsed.port().ok_or_else(|| {
        AppServerClientError::InvalidEndpoint(
            "app-server loopback websocket requires an explicit port".to_string(),
        )
    })?;
    let host = parsed.host().ok_or_else(|| {
        AppServerClientError::InvalidEndpoint("app-server websocket omitted host".to_string())
    })?;
    let addresses = match host {
        Host::Ipv4(address) if address.is_loopback() => {
            vec![SocketAddr::new(IpAddr::V4(address), port)]
        }
        Host::Ipv6(address) if address.is_loopback() => {
            vec![SocketAddr::new(IpAddr::V6(address), port)]
        }
        Host::Domain(domain) if domain.eq_ignore_ascii_case("localhost") => (domain, port)
            .to_socket_addrs()
            .map_err(|error| AppServerClientError::InvalidEndpoint(error.to_string()))?
            .filter(|address| address.ip().is_loopback())
            .collect::<Vec<_>>(),
        Host::Ipv4(_) | Host::Ipv6(_) | Host::Domain(_) => {
            return Err(non_loopback_endpoint_error());
        }
    };
    if addresses.is_empty() {
        return Err(non_loopback_endpoint_error());
    }
    Ok(addresses)
}

fn non_loopback_endpoint_error() -> AppServerClientError {
    AppServerClientError::InvalidEndpoint(
        "cutex only permits local loopback app-server websocket endpoints".to_string(),
    )
}

fn run_actor(
    mut socket: WebSocket<ConnectionStream>,
    command_rx: Receiver<ActorCommand>,
    event_tx: SyncSender<AppServerEvent>,
) {
    let mut pending = HashMap::<RpcRequestIdKey, PendingRequest>::new();
    let mut queued_command = None;
    let mut socket_read_timeout = IDLE_SOCKET_PROBE_TIMEOUT;
    let disconnect_reason = 'actor: loop {
        for _ in 0..MAX_COMMANDS_PER_TICK {
            let command = match queued_command.take() {
                Some(command) => Ok(command),
                None => command_rx.try_recv(),
            };
            match command {
                Ok(ActorCommand::Request {
                    id,
                    method,
                    message,
                    deadline,
                    response_tx,
                    response_observer,
                }) => {
                    if deadline <= Instant::now() {
                        let _ = response_tx.try_send(Err(AppServerClientError::Timeout { method }));
                        continue;
                    }
                    let request_id = match RpcRequestIdKey::from_value(&id) {
                        Ok(request_id) => request_id,
                        Err(error) => {
                            let _ = response_tx.try_send(Err(error));
                            continue;
                        }
                    };
                    if pending.contains_key(&request_id) {
                        let _ = response_tx.try_send(Err(AppServerClientError::RequestIdInUse(
                            format!("app-server request id {} is already pending", id),
                        )));
                        continue;
                    }
                    pending.insert(
                        request_id,
                        PendingRequest {
                            method,
                            deadline,
                            response_tx,
                            response_observer,
                            timed_out: false,
                        },
                    );
                    if let Err(error) = send_json(&mut socket, &message) {
                        break 'actor error.to_string();
                    }
                }
                Ok(ActorCommand::Send(message)) => {
                    if let Err(error) = send_json(&mut socket, &message) {
                        break 'actor error.to_string();
                    }
                }
                Ok(ActorCommand::Write {
                    message,
                    response_tx,
                }) => match send_json(&mut socket, &message) {
                    Ok(()) => {
                        let _ = response_tx.try_send(Ok(()));
                    }
                    Err(error) => {
                        let reason = error.to_string();
                        let _ = response_tx.try_send(Err(error));
                        break 'actor reason;
                    }
                },
                Ok(ActorCommand::Shutdown) => break 'actor "client shutdown".to_string(),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    break 'actor "all app-server handles were dropped".to_string();
                }
            }
        }

        expire_pending_requests(&mut pending);
        let desired_read_timeout = if pending.values().all(|request| request.timed_out) {
            IDLE_SOCKET_PROBE_TIMEOUT
        } else {
            ACTIVE_SOCKET_POLL_INTERVAL
        };
        if socket_read_timeout != desired_read_timeout {
            if let Err(error) = socket
                .get_ref()
                .set_read_timeout(Some(desired_read_timeout))
            {
                break 'actor error.to_string();
            }
            socket_read_timeout = desired_read_timeout;
        }
        let socket_activity = match socket.read() {
            Ok(Message::Text(text)) => {
                let raw = match serde_json::from_str::<Value>(text.as_ref()) {
                    Ok(raw) => raw,
                    Err(error) => {
                        if !emit_event(
                            &event_tx,
                            AppServerEvent::ProtocolViolation {
                                message: format!("invalid app-server JSON: {error}"),
                            },
                        ) {
                            break 'actor "app-server event consumer disconnected".to_string();
                        }
                        continue;
                    }
                };
                match classify_inbound(raw) {
                    Ok(InboundMessage::Response(response)) => {
                        let id = response.id.clone();
                        let request_id = match RpcRequestIdKey::from_value(&id) {
                            Ok(request_id) => request_id,
                            Err(error) => {
                                if !emit_event(
                                    &event_tx,
                                    AppServerEvent::ProtocolViolation {
                                        message: error.to_string(),
                                    },
                                ) {
                                    break 'actor "app-server event consumer disconnected"
                                        .to_string();
                                }
                                continue;
                            }
                        };
                        let Some(mut pending_request) = pending.remove(&request_id) else {
                            if !emit_event(
                                &event_tx,
                                AppServerEvent::ProtocolViolation {
                                    message: format!(
                                        "app-server returned response for unknown request id {id}"
                                    ),
                                },
                            ) {
                                break 'actor "app-server event consumer disconnected".to_string();
                            }
                            continue;
                        };
                        if let Some(observer) = pending_request.response_observer.take() {
                            observer(&response);
                        }
                        let _ = pending_request.response_tx.try_send(Ok(response));
                    }
                    Ok(InboundMessage::Notification(notification)) => {
                        if !emit_event(&event_tx, AppServerEvent::Notification(notification)) {
                            break 'actor "app-server event consumer disconnected".to_string();
                        }
                    }
                    Ok(InboundMessage::ServerRequest(request)) => {
                        if !emit_event(&event_tx, AppServerEvent::ServerRequest(request)) {
                            break 'actor "app-server event consumer disconnected".to_string();
                        }
                    }
                    Err(error) => {
                        if !emit_event(
                            &event_tx,
                            AppServerEvent::ProtocolViolation {
                                message: error.to_string(),
                            },
                        ) {
                            break 'actor "app-server event consumer disconnected".to_string();
                        }
                    }
                }
                true
            }
            Ok(Message::Ping(payload)) => {
                if let Err(error) = socket.send(Message::Pong(payload)) {
                    break 'actor error.to_string();
                }
                true
            }
            Ok(Message::Close(frame)) => {
                break 'actor format!("app-server closed the websocket: {frame:?}");
            }
            Ok(Message::Binary(_)) | Ok(Message::Frame(_)) => {
                if !emit_event(
                    &event_tx,
                    AppServerEvent::ProtocolViolation {
                        message: "app-server sent an unsupported non-text frame".to_string(),
                    },
                ) {
                    break 'actor "app-server event consumer disconnected".to_string();
                }
                true
            }
            Ok(Message::Pong(_)) => true,
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                false
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                break 'actor "app-server websocket closed".to_string();
            }
            Err(error) => break 'actor error.to_string(),
        };
        if pending.values().all(|request| request.timed_out) && !socket_activity {
            match command_rx.recv_timeout(ACTOR_POLL_INTERVAL) {
                Ok(command) => queued_command = Some(command),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    break 'actor "all app-server handles were dropped".to_string();
                }
            }
        }
    };

    for (_, request) in pending.drain() {
        let _ = request
            .response_tx
            .try_send(Err(AppServerClientError::Disconnected(
                disconnect_reason.clone(),
            )));
    }
    let _ = event_tx.try_send(AppServerEvent::Disconnected {
        reason: disconnect_reason,
    });
}

fn send_json(
    socket: &mut WebSocket<ConnectionStream>,
    message: &Value,
) -> Result<(), AppServerClientError> {
    let text = serde_json::to_string(message)
        .map_err(|error| AppServerClientError::Protocol(error.to_string()))?;
    socket
        .send(Message::Text(text.into()))
        .map_err(|error| AppServerClientError::Transport(error.to_string()))
}

fn expire_pending_requests(pending: &mut HashMap<RpcRequestIdKey, PendingRequest>) {
    let now = Instant::now();
    let expired = pending
        .iter()
        .filter_map(|(id, request)| {
            (!request.timed_out && request.deadline <= now).then_some(id.clone())
        })
        .collect::<Vec<_>>();
    for id in expired {
        if let Some(mut request) = pending.remove(&id) {
            let _ = request
                .response_tx
                .try_send(Err(AppServerClientError::Timeout {
                    method: request.method.clone(),
                }));
            if request.response_observer.is_some() {
                request.timed_out = true;
                pending.insert(id, request);
            }
        }
    }
}

fn emit_event(event_tx: &SyncSender<AppServerEvent>, event: AppServerEvent) -> bool {
    // Native output deltas can arrive faster than the management projection can
    // durably append them. Block the socket reader to apply transport
    // backpressure instead of treating a full local queue as a disconnection.
    event_tx.send(event).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn windows_test_socket_pair() -> (WebSocket<ConnectionStream>, WebSocket<TcpStream>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let stream = ConnectionStream::tcp(client);
        stream.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
        let ConnectionStream::Tcp { stream: tcp, .. } = &stream;
        assert_eq!(tcp.read_timeout().unwrap(), None, "Winsock recv must never time out during polling");
        server.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        (
            WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Client, None),
            WebSocket::from_raw_socket(server, tungstenite::protocol::Role::Server, None),
        )
    }

    #[cfg(windows)]
    fn assert_windows_idle(socket: &mut WebSocket<ConnectionStream>) {
        assert!(matches!(socket.read(), Err(tungstenite::Error::Io(error))
            if error.kind() == io::ErrorKind::WouldBlock));
    }

    #[cfg(windows)]
    fn windows_read_test_message(socket: &mut WebSocket<ConnectionStream>) -> tungstenite::Result<Message> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match socket.read() {
                Err(tungstenite::Error::Io(error))
                    if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {}
                result => return result,
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_readiness_survives_idle_polls_then_notification_and_rpc() {
        let (mut client, mut server) = windows_test_socket_pair();
        for _ in 0..20 {
            assert_windows_idle(&mut client);
        }
        let notification = json!({"method": "test/notification", "params": {"ok": true}});
        server.send(Message::Text(notification.to_string().into())).unwrap();
        let message = windows_read_test_message(&mut client).unwrap().into_text().unwrap();
        assert_eq!(serde_json::from_str::<Value>(&message).unwrap(), notification);
        let request = json!({"id": 7, "method": "test/rpc", "params": {}});
        client.send(Message::Text(request.to_string().into())).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&server.read().unwrap().into_text().unwrap()).unwrap(), request);
        let response = json!({"id": 7, "result": "ready"});
        server.send(Message::Text(response.to_string().into())).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&windows_read_test_message(&mut client).unwrap().into_text().unwrap()).unwrap(), response);
    }

    #[cfg(windows)]
    #[test]
    fn windows_readiness_preserves_partial_and_fragmented_frames_across_polls() {
        let (mut client, mut server) = windows_test_socket_pair();
        // An incomplete header, then a complete non-final text fragment,
        // each separated by repeated idle reads, must retain parser state.
        server.get_mut().write_all(&[0x01]).unwrap();
        for _ in 0..3 { assert_windows_idle(&mut client); }
        server.get_mut().write_all(&[3, b'a', b'b', b'c']).unwrap();
        for _ in 0..3 { assert_windows_idle(&mut client); }
        // The final continuation's body also arrives in separate reads.
        server.get_mut().write_all(&[0x80, 3, b'd']).unwrap();
        for _ in 0..3 { assert_windows_idle(&mut client); }
        server.get_mut().write_all(b"ef").unwrap();
        assert_eq!(windows_read_test_message(&mut client).unwrap().into_text().unwrap().as_str(), "abcdef");
    }

    #[cfg(windows)]
    #[test]
    fn windows_readiness_reports_eof_instead_of_idle() {
        let (mut client, server) = windows_test_socket_pair();
        assert_windows_idle(&mut client);
        server.get_ref().shutdown(std::net::Shutdown::Both).unwrap();
        drop(server);
        match windows_read_test_message(&mut client) {
            Err(tungstenite::Error::Io(error)) => {
                assert!(!matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut));
            }
            Err(_) => {}
            Ok(message) => panic!("EOF must close the transport, got {message:?}"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_readiness_timeout_conversion_and_handshake_deadline() {
        let tiny = windows_select_timeout(Duration::from_nanos(1)).unwrap();
        assert_eq!((tiny.tv_sec, tiny.tv_usec), (0, 1));
        let carry = windows_select_timeout(Duration::from_nanos(999_999_999)).unwrap();
        assert_eq!((carry.tv_sec, carry.tv_usec), (1, 0));
        assert!(windows_select_timeout(Duration::from_secs(i32::MAX as u64 + 1)).is_err());
        let (mut client, _server) = windows_test_socket_pair();
        let ConnectionStream::Tcp { handshake_deadline, .. } = client.get_ref();
        handshake_deadline.set(Some(Instant::now()));
        assert!(matches!(client.read(), Err(tungstenite::Error::Io(error))
            if error.kind() == io::ErrorKind::TimedOut));
        assert!(client.get_ref().set_read_timeout(Some(Duration::ZERO)).is_err());
    }

    #[test]
    fn initialize_sends_extra_capabilities_without_overriding_standard_ones() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let port = listener.local_addr().expect("listener address").port();
        fn next_text(socket: &mut WebSocket<TcpStream>) -> Value {
            loop {
                if let Message::Text(text) = socket.read().expect("read websocket") {
                    return serde_json::from_str(text.as_ref()).expect("parse JSON");
                }
            }
        }
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            let mut socket = tungstenite::accept(stream).expect("accept websocket");
            let initialize = next_text(&mut socket);
            let reply = json!({ "id": initialize["id"].clone(), "result": { "userAgent": "test" } });
            socket
                .send(Message::Text(reply.to_string().into()))
                .expect("write initialize response");
            assert_eq!(next_text(&mut socket)["method"], "initialized");
            initialize
        });
        let mut options = AppServerClientOptions::new(AppServerEndpoint::LoopbackWebSocket {
            url: format!("ws://127.0.0.1:{port}/"),
            bearer_token: None,
        });
        options.request_timeout = Duration::from_secs(5);
        options
            .extra_capabilities
            .insert("claudeInteractiveApprovals".to_string(), Value::Bool(true));
        options
            .extra_capabilities
            .insert("experimentalApi".to_string(), Value::Bool(false));
        let client = AppServerClient::connect(options).expect("connect client");
        let initialize = server.join().expect("server thread");
        assert_eq!(
            initialize["params"]["capabilities"],
            json!({
                "claudeInteractiveApprovals": true,
                "experimentalApi": true,
                "optOutNotificationMethods": [],
            })
        );
        drop(client);
    }

    #[test]
    fn websocket_endpoint_rejects_non_loopback_hosts() {
        for url in [
            "ws://example.com:1234/",
            "ws://10.0.0.5:1234/",
            "wss://localhost:1234/",
            "ws://localhost/",
            "ws://localhost:1234/path",
            "ws://user@localhost:1234/",
            "ws://localhost:1234/#fragment",
        ] {
            assert!(loopback_websocket_addresses(url).is_err(), "{url}");
        }
        assert!(loopback_websocket_addresses("ws://127.0.0.1:1234/").is_ok());
        assert!(loopback_websocket_addresses("ws://[::1]:1234/").is_ok());
    }

    #[test]
    fn handle_rejects_empty_methods_and_request_id_exhaustion() {
        let (command_tx, _command_rx) = mpsc::sync_channel(1);
        let handle = AppServerHandle {
            command_tx,
            next_id: Arc::new(AtomicU64::new(u64::MAX)),
            default_timeout: Duration::from_secs(1),
        };

        assert!(matches!(
            handle.notify("", None),
            Err(AppServerClientError::Protocol(_))
        ));
        assert!(matches!(
            handle.request("thread/read", json!({ "threadId": "thread-1" })),
            Err(AppServerClientError::InvalidOptions(message))
                if message.contains("request id space")
        ));
    }

    #[test]
    fn client_drop_unblocks_actor_when_event_queue_is_saturated() {
        let (client, send_attempted_rx) = saturated_event_client_for_test();
        send_attempted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor attempted saturated event send");

        let (drop_complete_tx, drop_complete_rx) = mpsc::sync_channel(1);
        let dropper = thread::spawn(move || {
            drop(client);
            drop_complete_tx
                .send(())
                .expect("signal completed client drop");
        });
        drop_complete_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("client drop must release a saturated event sender");
        dropper.join().expect("client drop thread");
    }

    #[cfg(unix)]
    #[test]
    fn actor_correlates_responses_and_preserves_unknown_events_and_requests() {
        use std::fs;
        use std::os::unix::net::UnixListener;

        let directory = std::env::temp_dir().join(format!(
            "cutex-app-server-client-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&directory).expect("create test directory");
        let socket_path = directory.join("app-server.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            let mut socket = tungstenite::accept(stream).expect("accept websocket");

            let initialize = read_json(&mut socket);
            assert_eq!(initialize["method"], "initialize");
            let initialize_id = initialize["id"].clone();
            write_json(
                &mut socket,
                json!({ "id": initialize_id, "result": { "userAgent": "test" } }),
            );
            assert_eq!(read_json(&mut socket)["method"], "initialized");

            let request = read_json(&mut socket);
            assert_eq!(request["method"], "thread/read");
            write_json(
                &mut socket,
                json!({
                    "method": "future/native/event",
                    "params": { "threadId": "thread-1", "unknown": true }
                }),
            );
            write_json(
                &mut socket,
                json!({ "id": request["id"].clone(), "result": { "thread": { "id": "thread-1" } } }),
            );
            write_json(
                &mut socket,
                json!({
                    "id": "approval-1",
                    "method": "item/commandExecution/requestApproval",
                    "params": { "threadId": "thread-1", "itemId": "item-1" }
                }),
            );
            let response = read_json(&mut socket);
            assert_eq!(response["id"], "approval-1");
            assert_eq!(response["result"]["decision"], "accept");
            assert!(response.get("futureResponseField").is_some());
        });

        let mut options = AppServerClientOptions::new(AppServerEndpoint::UnixSocket {
            socket_path: socket_path.clone(),
        });
        options.request_timeout = Duration::from_secs(2);
        let client = AppServerClient::connect(options).expect("connect client");
        assert_eq!(client.initialize_response()["userAgent"], "test");
        let response = client
            .handle()
            .request("thread/read", json!({ "threadId": "thread-1" }))
            .expect("thread/read response");
        assert_eq!(response["thread"]["id"], "thread-1");

        let notification = client
            .recv_event_timeout(Duration::from_secs(2))
            .expect("event receive")
            .expect("notification");
        let AppServerEvent::Notification(notification) = notification else {
            panic!("expected notification");
        };
        assert_eq!(notification.method, "future/native/event");
        assert_eq!(notification.raw["params"]["unknown"], true);

        let request = client
            .recv_event_timeout(Duration::from_secs(2))
            .expect("event receive")
            .expect("server request");
        let AppServerEvent::ServerRequest(request) = request else {
            panic!("expected server request");
        };
        client
            .handle()
            .respond_raw(json!({
                "id": request.id,
                "result": { "decision": "accept" },
                "futureResponseField": null
            }))
            .expect("respond to request");
        server.join().expect("server thread");
        drop(client);
        let _ = fs::remove_file(socket_path);
        fs::remove_dir(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn actor_correlates_concurrent_responses_returned_out_of_order() {
        use std::fs;
        use std::os::unix::net::UnixListener;

        let directory = std::env::temp_dir().join(format!(
            "cutex-app-server-concurrent-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&directory).expect("create test directory");
        let socket_path = directory.join("app-server.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            let mut socket = tungstenite::accept(stream).expect("accept websocket");
            initialize_test_connection(&mut socket);

            let first = read_json(&mut socket);
            let second = read_json(&mut socket);
            let mut requests = HashMap::new();
            requests.insert(
                first["method"].as_str().expect("first method").to_string(),
                first["id"].clone(),
            );
            requests.insert(
                second["method"]
                    .as_str()
                    .expect("second method")
                    .to_string(),
                second["id"].clone(),
            );
            write_json(
                &mut socket,
                json!({ "id": requests["thread/settings/update"].clone(), "result": { "kind": "settings" } }),
            );
            write_json(
                &mut socket,
                json!({ "method": "future/concurrent/event", "params": { "threadId": "thread-1" } }),
            );
            write_json(
                &mut socket,
                json!({ "id": requests["thread/read"].clone(), "result": { "kind": "read" } }),
            );
        });

        let mut options = AppServerClientOptions::new(AppServerEndpoint::UnixSocket {
            socket_path: socket_path.clone(),
        });
        options.request_timeout = Duration::from_secs(2);
        let client = AppServerClient::connect(options).expect("connect client");
        let read_handle = client.handle();
        let settings_handle = client.handle();
        let read_thread = thread::spawn(move || {
            read_handle.request("thread/read", json!({ "threadId": "thread-1" }))
        });
        let settings_thread = thread::spawn(move || {
            settings_handle.request(
                "thread/settings/update",
                json!({ "threadId": "thread-1", "effort": "low" }),
            )
        });
        assert_eq!(
            read_thread
                .join()
                .expect("read request thread")
                .expect("read response")["kind"],
            "read"
        );
        assert_eq!(
            settings_thread
                .join()
                .expect("settings request thread")
                .expect("settings response")["kind"],
            "settings"
        );
        let event = client
            .recv_event_timeout(Duration::from_secs(2))
            .expect("event receive")
            .expect("notification");
        assert!(matches!(
            event,
            AppServerEvent::Notification(RpcNotification { method, .. })
                if method == "future/concurrent/event"
        ));

        server.join().expect("server thread");
        drop(client);
        let _ = fs::remove_file(socket_path);
        fs::remove_dir(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn actor_preserves_native_ids_raw_responses_and_pending_id_uniqueness() {
        use std::fs;
        use std::os::unix::net::UnixListener;

        let directory = std::env::temp_dir().join(format!(
            "cutex-app-server-raw-request-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&directory).expect("create test directory");
        let socket_path = directory.join("app-server.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");
        let (duplicate_seen_tx, duplicate_seen_rx) = mpsc::sync_channel(1);
        let (release_duplicate_tx, release_duplicate_rx) = mpsc::sync_channel(1);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            let mut socket = tungstenite::accept(stream).expect("accept websocket");
            initialize_test_connection(&mut socket);

            let string_id = read_json(&mut socket);
            assert_eq!(string_id["id"], "backend-native-1");
            assert_eq!(string_id["futureRequestField"], json!({ "kept": true }));
            write_json(
                &mut socket,
                json!({
                    "id": "backend-native-1",
                    "result": { "thread": { "id": "thread-1" } },
                    "trace": { "future": true }
                }),
            );

            let signed_id = read_json(&mut socket);
            assert_eq!(signed_id["id"], i64::MIN);
            write_json(
                &mut socket,
                json!({
                    "id": i64::MIN,
                    "error": {
                        "code": -32001,
                        "message": "native request failed",
                        "data": { "retryAfterMs": 250 }
                    },
                    "futureTopLevel": [1, 2, 3]
                }),
            );

            let duplicate = read_json(&mut socket);
            assert_eq!(duplicate["id"], "duplicate-id");
            duplicate_seen_tx
                .send(())
                .expect("signal duplicate request");
            release_duplicate_rx
                .recv()
                .expect("release duplicate request");
            write_json(
                &mut socket,
                json!({ "id": "duplicate-id", "result": { "accepted": true } }),
            );
        });

        let mut options = AppServerClientOptions::new(AppServerEndpoint::UnixSocket {
            socket_path: socket_path.clone(),
        });
        options.request_timeout = Duration::from_secs(2);
        let client = AppServerClient::connect(options).expect("connect client");
        let handle = client.handle();

        let success = handle
            .request_raw_message(json!({
                "id": "backend-native-1",
                "method": "thread/read",
                "params": { "threadId": "thread-1" },
                "futureRequestField": { "kept": true }
            }))
            .expect("raw success response");
        assert_eq!(success["id"], "backend-native-1");
        assert_eq!(success["trace"]["future"], true);

        let error = handle
            .request_raw(
                json!(i64::MIN),
                "thread/read",
                json!({ "threadId": "thread-1" }),
            )
            .expect("raw error response");
        assert_eq!(error["id"], i64::MIN);
        assert_eq!(error["error"]["code"], -32001);
        assert_eq!(error["futureTopLevel"], json!([1, 2, 3]));

        let first_handle = handle.clone();
        let first = thread::spawn(move || {
            first_handle.request_raw(
                json!("duplicate-id"),
                "thread/read",
                json!({ "threadId": "thread-1" }),
            )
        });
        duplicate_seen_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("server observed first duplicate id");
        let duplicate_error = handle
            .request_raw(
                json!("duplicate-id"),
                "thread/read",
                json!({ "threadId": "thread-2" }),
            )
            .expect_err("second pending request id must be rejected");
        assert!(matches!(
            duplicate_error,
            AppServerClientError::RequestIdInUse(message)
                if message.contains("duplicate-id")
        ));
        release_duplicate_tx
            .send(())
            .expect("release first request");
        assert_eq!(
            first
                .join()
                .expect("first request thread")
                .expect("first request response")["result"]["accepted"],
            true
        );

        server.join().expect("server thread");
        drop(client);
        let _ = fs::remove_file(socket_path);
        fs::remove_dir(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn actor_applies_backpressure_without_disconnecting_on_event_burst() {
        use std::fs;
        use std::os::unix::net::UnixListener;

        const EVENT_COUNT: usize = 16;

        let directory = std::env::temp_dir().join(format!(
            "cutex-as-backpressure-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        fs::create_dir(&directory).expect("create test directory");
        let socket_path = directory.join("app-server.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            let mut socket = tungstenite::accept(stream).expect("accept websocket");
            initialize_test_connection(&mut socket);
            for index in 0..EVENT_COUNT {
                write_json(
                    &mut socket,
                    json!({
                        "method": "item/commandExecution/outputDelta",
                        "params": { "threadId": "thread-1", "index": index }
                    }),
                );
            }
            let request = read_json(&mut socket);
            assert_eq!(request["method"], "thread/read");
            write_json(
                &mut socket,
                json!({ "id": request["id"].clone(), "result": { "thread": { "id": "thread-1" } } }),
            );
        });

        let mut options = AppServerClientOptions::new(AppServerEndpoint::UnixSocket {
            socket_path: socket_path.clone(),
        });
        options.event_capacity = 1;
        options.request_timeout = Duration::from_secs(2);
        let client = AppServerClient::connect(options).expect("connect client");

        thread::sleep(Duration::from_millis(100));
        for expected_index in 0..EVENT_COUNT {
            let event = client
                .recv_event_timeout(Duration::from_secs(2))
                .expect("event receive")
                .expect("notification");
            let AppServerEvent::Notification(notification) = event else {
                panic!("expected notification");
            };
            assert_eq!(notification.method, "item/commandExecution/outputDelta");
            assert_eq!(
                notification
                    .params
                    .as_ref()
                    .and_then(|params| params["index"].as_u64()),
                Some(expected_index as u64)
            );
        }
        let response = client
            .handle()
            .request("thread/read", json!({ "threadId": "thread-1" }))
            .expect("request after event burst");
        assert_eq!(response["thread"]["id"], "thread-1");

        server.join().expect("server thread");
        drop(client);
        let _ = fs::remove_file(socket_path);
        fs::remove_dir(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn actor_times_out_pending_request_deterministically() {
        use std::fs;
        use std::os::unix::net::UnixListener;

        let directory = std::env::temp_dir().join(format!(
            "cutex-app-server-timeout-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&directory).expect("create test directory");
        let socket_path = directory.join("app-server.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            let mut socket = tungstenite::accept(stream).expect("accept websocket");
            initialize_test_connection(&mut socket);
            assert_eq!(read_json(&mut socket)["method"], "thread/read");
            thread::sleep(Duration::from_millis(300));
        });

        let mut options = AppServerClientOptions::new(AppServerEndpoint::UnixSocket {
            socket_path: socket_path.clone(),
        });
        options.request_timeout = Duration::from_millis(100);
        let client = AppServerClient::connect(options).expect("connect client");
        let error = client
            .handle()
            .request("thread/read", json!({ "threadId": "thread-1" }))
            .expect_err("request should time out");
        assert_eq!(
            error,
            AppServerClientError::Timeout {
                method: "thread/read".to_string()
            }
        );

        drop(client);
        server.join().expect("server thread");
        let _ = fs::remove_file(socket_path);
        fs::remove_dir(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    fn initialize_test_connection(socket: &mut WebSocket<UnixStream>) {
        let initialize = read_json(socket);
        assert_eq!(initialize["method"], "initialize");
        write_json(
            socket,
            json!({ "id": initialize["id"].clone(), "result": { "userAgent": "test" } }),
        );
        assert_eq!(read_json(socket)["method"], "initialized");
    }

    #[cfg(unix)]
    fn read_json(socket: &mut WebSocket<UnixStream>) -> Value {
        loop {
            match socket.read().expect("read websocket") {
                Message::Text(text) => {
                    return serde_json::from_str(text.as_ref()).expect("parse websocket JSON");
                }
                Message::Ping(payload) => socket
                    .send(Message::Pong(payload))
                    .expect("send websocket pong"),
                message => panic!("unexpected websocket message: {message:?}"),
            }
        }
    }

    #[cfg(unix)]
    fn write_json(socket: &mut WebSocket<UnixStream>, value: Value) {
        socket
            .send(Message::Text(
                serde_json::to_string(&value)
                    .expect("serialize websocket JSON")
                    .into(),
            ))
            .expect("write websocket");
    }
}
