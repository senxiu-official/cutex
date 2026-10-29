//! Owner-only PTY terminals rooted at a workspace (ConPTY on Windows).
//!
//! Terminals live in the daemon process: they survive client disconnects and
//! end when killed (`DELETE`), when the shell exits, or when the daemon stops.
//! Output is kept in a bounded scrollback ring and streamed over SSE with
//! byte-offset event IDs so a client can reattach without gaps.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use base64::Engine;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;

use super::workspace::ResolvedRoot;
use super::workspace::WorkspaceError;
use super::workspace::WorkspaceResult;

pub const MAX_LIVE_TERMINALS: usize = 16;
const MAX_RETAINED_TERMINALS: usize = 32;
pub const SCROLLBACK_BYTES: usize = 256 * 1024;
const SUBSCRIBER_CAPACITY: usize = 512;
const SSE_KEEPALIVE: Duration = Duration::from_secs(15);
const MAX_INPUT_BYTES: usize = 1024 * 1024;
/// `DELETE` hangs the shell up first (SIGHUP, so it can pass the hangup on to
/// its jobs); a shell still running after this grace is killed with its
/// process group (Unix: an interactive shell can miss a SIGHUP that arrives
/// while it starts up, or a script may ignore it).
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_millis(1500);
/// After the shell exited, output still arriving is drained until the pty is
/// quiet for one poll interval, at most this long. Unix: other processes may
/// hold the pty slave open (a background job, or a process spawned elsewhere
/// in the daemon while `openpty` had not yet marked the slave close-on-exec),
/// so EOF on the master can be arbitrarily late.
#[cfg(unix)]
const EXIT_DRAIN_MAX: Duration = Duration::from_secs(1);
#[cfg(unix)]
const READER_POLL_MS: i32 = 100;

#[derive(Clone)]
enum TerminalEvent {
    /// `(end offset after this chunk, bytes)`.
    Output(u64, Arc<Vec<u8>>),
    Exit(Option<u32>),
}

struct TerminalState {
    scrollback: VecDeque<u8>,
    /// Total bytes produced so far; the ring holds `[end - len, end)`.
    end_offset: u64,
    exited: bool,
    exit_code: Option<u32>,
    subscribers: Vec<mpsc::SyncSender<TerminalEvent>>,
    cols: u16,
    rows: u16,
}

struct Terminal {
    id: String,
    scope_kind: &'static str,
    root_id: String,
    cwd: String,
    shell: String,
    pid: Option<u32>,
    created_at: String,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>,
    killer: Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
    /// Held by the waiter while it records the shell's exit and reaps it, and
    /// by the kill escalation while it signals: the escalation never signals a
    /// reaped (possibly reused) pid.
    child_exited: Mutex<bool>,
    state: Mutex<TerminalState>,
}

impl Terminal {
    fn summary(&self) -> Value {
        let state = self.state.lock().expect("terminal state");
        json!({
            "terminalId": self.id,
            "scope": self.scope_kind,
            "rootId": self.root_id,
            "cwd": self.cwd,
            "shell": self.shell,
            "pid": self.pid,
            "cols": state.cols,
            "rows": state.rows,
            "createdAt": self.created_at,
            "exited": state.exited,
            "exitCode": state.exit_code,
        })
    }

    fn push_output(&self, bytes: &[u8]) {
        let mut state = self.state.lock().expect("terminal state");
        state.scrollback.extend(bytes);
        let overflow = state.scrollback.len().saturating_sub(SCROLLBACK_BYTES);
        if overflow > 0 {
            state.scrollback.drain(..overflow);
        }
        state.end_offset += bytes.len() as u64;
        let event = TerminalEvent::Output(state.end_offset, Arc::new(bytes.to_vec()));
        // A subscriber that cannot keep up is dropped; it reattaches and
        // resumes from its last offset via the scrollback.
        state
            .subscribers
            .retain(|subscriber| subscriber.try_send(event.clone()).is_ok());
    }

    fn mark_exited(&self, code: Option<u32>) {
        let mut state = self.state.lock().expect("terminal state");
        if state.exited {
            return;
        }
        state.exited = true;
        state.exit_code = code;
        for subscriber in state.subscribers.drain(..) {
            let _ = subscriber.try_send(TerminalEvent::Exit(code));
        }
    }
}

fn registry() -> &'static Mutex<HashMap<String, Arc<Terminal>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<Terminal>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn get(id: &str) -> WorkspaceResult<Arc<Terminal>> {
    registry()
        .lock()
        .map_err(|_| WorkspaceError::internal("terminal registry poisoned"))?
        .get(id)
        .cloned()
        .ok_or_else(|| WorkspaceError::new(404, "terminal_not_found", "no such terminal"))
}

fn sorted_summaries(filter: impl Fn(&Terminal) -> bool) -> Vec<Value> {
    let terminals: Vec<Arc<Terminal>> = registry()
        .lock()
        .map(|registry| registry.values().cloned().collect())
        .unwrap_or_default();
    let mut terminals: Vec<Arc<Terminal>> = terminals
        .into_iter()
        .filter(|terminal| filter(terminal))
        .collect();
    terminals.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    terminals
        .iter()
        .map(|terminal| terminal.summary())
        .collect()
}

/// `GET /v2/terminals`.
pub fn list_resource() -> Value {
    json!({ "terminals": sorted_summaries(|_| true), "maxLive": MAX_LIVE_TERMINALS })
}

/// `GET {scope}/terminals`: only terminals created in that scope.
pub fn list_resource_for(root: &ResolvedRoot) -> Value {
    let kind = root.scope.kind.label();
    json!({
        "terminals": sorted_summaries(|terminal| {
            terminal.scope_kind == kind && terminal.root_id == root.scope.id
        }),
        "maxLive": MAX_LIVE_TERMINALS,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateRequest {
    cols: u16,
    rows: u16,
    #[serde(default)]
    shell: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputRequest {
    data: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResizeRequest {
    cols: u16,
    rows: u16,
}

fn parse_body<T: for<'de> Deserialize<'de>>(body: &[u8]) -> WorkspaceResult<T> {
    serde_json::from_slice(body)
        .map_err(|error| WorkspaceError::bad_request(format!("invalid request body: {error}")))
}

fn validate_size(cols: u16, rows: u16) -> WorkspaceResult<()> {
    if (1..=1000).contains(&cols) && (1..=1000).contains(&rows) {
        Ok(())
    } else {
        Err(WorkspaceError::bad_request(
            "cols and rows must be between 1 and 1000",
        ))
    }
}

fn find_in_path(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// `(program, args)` for the user's default interactive shell.
pub fn default_shell() -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        for program in ["pwsh.exe", "powershell.exe"] {
            if let Some(path) = find_in_path(program) {
                return (
                    path.to_string_lossy().into_owned(),
                    vec!["-NoLogo".to_string()],
                );
            }
        }
        let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
        (comspec, Vec::new())
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.is_empty())
            .or_else(|| find_in_path("bash").map(|path| path.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "/bin/sh".to_string());
        (shell, Vec::new())
    }
}

/// `POST {scope}/terminals` body `{cols, rows, shell?}`.
pub fn create(root: &ResolvedRoot, body: &[u8]) -> WorkspaceResult<Value> {
    let request: CreateRequest = parse_body(body)?;
    validate_size(request.cols, request.rows)?;
    let (program, args) = match request.shell.as_deref().map(str::trim) {
        Some(shell) if !shell.is_empty() => (shell.to_string(), Vec::new()),
        Some(_) => return Err(WorkspaceError::bad_request("shell must not be empty")),
        None => default_shell(),
    };
    {
        let mut registry = registry()
            .lock()
            .map_err(|_| WorkspaceError::internal("terminal registry poisoned"))?;
        let live = registry
            .values()
            .filter(|terminal| {
                !terminal
                    .state
                    .lock()
                    .map(|state| state.exited)
                    .unwrap_or(true)
            })
            .count();
        if live >= MAX_LIVE_TERMINALS {
            return Err(WorkspaceError::new(
                429,
                "terminal_limit_reached",
                format!("at most {MAX_LIVE_TERMINALS} terminals may run at once"),
            ));
        }
        // Forget the oldest exited terminals beyond the retention cap.
        if registry.len() >= MAX_RETAINED_TERMINALS {
            let mut exited: Vec<(String, String)> = registry
                .values()
                .filter(|terminal| {
                    terminal
                        .state
                        .lock()
                        .map(|state| state.exited)
                        .unwrap_or(true)
                })
                .map(|terminal| (terminal.created_at.clone(), terminal.id.clone()))
                .collect();
            exited.sort();
            let excess = registry.len() + 1 - MAX_RETAINED_TERMINALS;
            for (_, id) in exited.into_iter().take(excess) {
                registry.remove(&id);
            }
        }
    }

    let size = portable_pty::PtySize {
        rows: request.rows,
        cols: request.cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = portable_pty::native_pty_system()
        .openpty(size)
        .map_err(|error| WorkspaceError::new(503, "pty_unavailable", format!("{error:#}")))?;
    let mut command = portable_pty::CommandBuilder::new(&program);
    command.args(&args);
    command.cwd(&root.display);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| WorkspaceError::new(400, "shell_spawn_failed", format!("{error:#}")))?;
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| WorkspaceError::new(503, "pty_unavailable", format!("{error:#}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| WorkspaceError::new(503, "pty_unavailable", format!("{error:#}")))?;
    let killer = child.clone_killer();
    let pid = child.process_id();
    #[cfg(unix)]
    let poll_fd = pair.master.as_raw_fd();
    #[cfg(not(unix))]
    let poll_fd: Option<i32> = None;
    let id = format!("term-{}", uuid::Uuid::new_v4().simple());
    let terminal = Arc::new(Terminal {
        id: id.clone(),
        scope_kind: root.scope.kind.label(),
        root_id: root.scope.id.clone(),
        cwd: root.display.clone(),
        shell: program,
        pid,
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        writer: Mutex::new(writer),
        master: Mutex::new(Some(pair.master)),
        killer: Mutex::new(killer),
        child_exited: Mutex::new(false),
        state: Mutex::new(TerminalState {
            scrollback: VecDeque::new(),
            end_offset: 0,
            exited: false,
            exit_code: None,
            subscribers: Vec::new(),
            cols: request.cols,
            rows: request.rows,
        }),
    });
    registry()
        .lock()
        .map_err(|_| WorkspaceError::internal("terminal registry poisoned"))?
        .insert(id.clone(), terminal.clone());

    let reader_terminal = terminal.clone();
    let stop_reading = Arc::new(AtomicBool::new(false));
    let reader_stop = stop_reading.clone();
    let reader_thread = std::thread::Builder::new()
        .name(format!("cutex-pty-read-{id}"))
        .spawn(move || read_loop(reader, reader_terminal, poll_fd, reader_stop))
        .map_err(WorkspaceError::internal)?;
    let waiter_terminal = terminal.clone();
    std::thread::Builder::new()
        .name(format!("cutex-pty-wait-{id}"))
        .spawn(move || {
            // Wait without reaping, then reap under the lock (see child_exited).
            #[cfg(unix)]
            if let Some(pid) = pid {
                wait_exited_no_reap(pid);
            }
            let code = {
                let mut exited = waiter_terminal
                    .child_exited
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let code = child.wait().ok().map(|status| status.exit_code());
                *exited = true;
                code
            };
            // Give the reader a moment to drain what the shell wrote last.
            std::thread::sleep(Duration::from_millis(150));
            stop_reading.store(true, Ordering::SeqCst);
            // ConPTY keeps the output pipe open until the pseudoconsole is
            // closed: close it first there. Unix: the reader polls the master
            // fd, so the master stays open until the reader is done.
            if poll_fd.is_none() {
                if let Ok(mut master) = waiter_terminal.master.lock() {
                    master.take();
                }
            }
            let _ = reader_thread.join();
            if let Ok(mut master) = waiter_terminal.master.lock() {
                master.take();
            }
            waiter_terminal.mark_exited(code);
        })
        .map_err(WorkspaceError::internal)?;
    Ok(terminal.summary())
}

/// Block until `pid` has exited, leaving it unreaped (`WNOWAIT`).
#[cfg(unix)]
fn wait_exited_no_reap(pid: u32) {
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
        {
            return;
        }
    }
}

/// Unix: `Some(true)` when the pty has data (or hung up), `Some(false)` on a
/// quiet poll interval, `None` when polling failed.
#[cfg(unix)]
fn poll_readable(fd: i32) -> Option<bool> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let result = unsafe { libc::poll(&mut pollfd, 1, READER_POLL_MS) };
        if result > 0 {
            return Some(true);
        }
        if result == 0 {
            return Some(false);
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return None;
        }
    }
}

/// Copies pty output into the terminal until EOF. Unix (`poll_fd` = the
/// master fd, open until this returns): once `stop` is set (the shell has
/// exited) it also ends when the pty is quiet or after [`EXIT_DRAIN_MAX`], so
/// a process still holding the slave cannot keep the terminal from exiting.
fn read_loop(
    mut reader: Box<dyn Read + Send>,
    terminal: Arc<Terminal>,
    poll_fd: Option<i32>,
    stop: Arc<AtomicBool>,
) {
    let mut buffer = vec![0_u8; 16 * 1024];
    #[cfg(unix)]
    let mut stopped_at: Option<std::time::Instant> = None;
    #[cfg(not(unix))]
    let _ = (&poll_fd, &stop);
    loop {
        #[cfg(unix)]
        if let Some(fd) = poll_fd {
            if stop.load(Ordering::SeqCst) {
                let since = *stopped_at.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= EXIT_DRAIN_MAX {
                    break;
                }
            }
            match poll_readable(fd) {
                Some(true) => {}
                Some(false) if stop.load(Ordering::SeqCst) => break,
                Some(false) => continue,
                None => break,
            }
        }
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => terminal.push_output(&buffer[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// `POST /v2/terminals/{id}/input` body `{data}` (UTF-8 text).
pub fn input(id: &str, body: &[u8]) -> WorkspaceResult<Value> {
    if body.len() > MAX_INPUT_BYTES {
        return Err(WorkspaceError::new(
            413,
            "payload_too_large",
            "terminal input is too large",
        ));
    }
    let request: InputRequest = parse_body(body)?;
    let terminal = get(id)?;
    if terminal
        .state
        .lock()
        .map(|state| state.exited)
        .unwrap_or(true)
    {
        return Err(WorkspaceError::new(
            409,
            "terminal_exited",
            "the terminal has exited",
        ));
    }
    let mut writer = terminal
        .writer
        .lock()
        .map_err(|_| WorkspaceError::internal("terminal writer poisoned"))?;
    writer
        .write_all(request.data.as_bytes())
        .and_then(|()| writer.flush())
        .map_err(|error| WorkspaceError::new(409, "terminal_write_failed", error.to_string()))?;
    Ok(json!({ "terminalId": id, "written": request.data.len() }))
}

/// `POST /v2/terminals/{id}/resize` body `{cols, rows}`.
pub fn resize(id: &str, body: &[u8]) -> WorkspaceResult<Value> {
    let request: ResizeRequest = parse_body(body)?;
    validate_size(request.cols, request.rows)?;
    let terminal = get(id)?;
    {
        let master = terminal
            .master
            .lock()
            .map_err(|_| WorkspaceError::internal("terminal poisoned"))?;
        let Some(master) = master.as_ref() else {
            return Err(WorkspaceError::new(
                409,
                "terminal_exited",
                "the terminal has exited",
            ));
        };
        master
            .resize(portable_pty::PtySize {
                rows: request.rows,
                cols: request.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| {
                WorkspaceError::new(409, "terminal_resize_failed", format!("{error:#}"))
            })?;
    }
    if let Ok(mut state) = terminal.state.lock() {
        state.cols = request.cols;
        state.rows = request.rows;
    }
    Ok(terminal.summary())
}

/// `DELETE /v2/terminals/{id}`: kill (if running) and forget the terminal.
pub fn kill(id: &str) -> WorkspaceResult<Value> {
    let terminal = get(id)?;
    let was_running = !terminal
        .state
        .lock()
        .map(|state| state.exited)
        .unwrap_or(true);
    if was_running {
        if let Ok(mut killer) = terminal.killer.lock() {
            let _ = killer.kill();
        }
        #[cfg(unix)]
        if let Some(pid) = terminal.pid {
            let terminal = terminal.clone();
            let _ = std::thread::Builder::new()
                .name(format!("cutex-pty-kill-{id}"))
                .spawn(move || escalate_kill(&terminal, pid));
        }
    }
    if let Ok(mut registry) = registry().lock() {
        registry.remove(id);
    }
    Ok(json!({ "terminalId": id, "killed": was_running }))
}

/// The shell is still running [`KILL_GRACE`] after the hangup: SIGKILL its
/// process group (the shell leads its own session and group) and the shell.
#[cfg(unix)]
fn escalate_kill(terminal: &Terminal, pid: u32) {
    let deadline = std::time::Instant::now() + KILL_GRACE;
    loop {
        {
            let exited = terminal
                .child_exited
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *exited {
                return;
            }
            if std::time::Instant::now() >= deadline {
                // Not reaped while this lock is held, so the pid is still ours.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Snapshot for (re)attachment: bytes after `since` when that offset is still
/// retained, else the whole ring (`reset: true`).
fn replay_from(state: &TerminalState, since: Option<u64>) -> (Vec<u8>, bool) {
    let start = state.end_offset - state.scrollback.len() as u64;
    match since {
        Some(since) if since >= start && since <= state.end_offset => {
            let skip = (since - start) as usize;
            (state.scrollback.iter().skip(skip).copied().collect(), false)
        }
        _ => (state.scrollback.iter().copied().collect(), true),
    }
}

/// `GET /v2/terminals/{id}/stream[?since=<offset>]` (or `Last-Event-ID`).
/// SSE events, each with `id: <end byte offset>` where applicable:
/// - `ready`  `{terminalId, cols, rows, offset}`
/// - `output` `{data: base64, replay?: true, reset?: true}` — `reset` means the
///   client must clear its screen before writing (gap could not be bridged)
/// - `exit`   `{exitCode}` then the stream closes.
pub fn stream(
    stream: &mut TcpStream,
    request: &crate::http::server::SimpleHttpRequest,
    id: &str,
) -> anyhow::Result<()> {
    let terminal = match get(id) {
        Ok(terminal) => terminal,
        Err(error) => return super::workspace::write_error(stream, &error),
    };
    let params = super::workspace::query_params(request);
    let since = params
        .get("since")
        .or_else(|| request.headers.get("last-event-id"))
        .and_then(|value| value.parse::<u64>().ok());
    let (receiver, replay, reset, offset, exited, exit_code, cols, rows) = {
        let mut state = terminal.state.lock().expect("terminal state");
        let (replay, reset) = replay_from(&state, since);
        let (sender, receiver) = mpsc::sync_channel(SUBSCRIBER_CAPACITY);
        if !state.exited {
            state.subscribers.push(sender);
        }
        (
            receiver,
            replay,
            reset,
            state.end_offset,
            state.exited,
            state.exit_code,
            state.cols,
            state.rows,
        )
    };
    stream.set_write_timeout(Some(Duration::from_secs(10))).ok();
    let engine = base64::engine::general_purpose::STANDARD;
    let _ = (|| -> anyhow::Result<()> {
        stream.write_all(
            crate::http::server::sse_response_head().as_bytes(),
        )?;
        super::workspace_fs::write_sse(
            stream,
            "ready",
            &json!({"terminalId": id, "cols": cols, "rows": rows, "offset": offset}),
        )?;
        if !replay.is_empty() || (reset && since.is_some()) {
            write_output(stream, offset, &engine.encode(&replay), true, reset)?;
        }
        if exited {
            super::workspace_fs::write_sse(stream, "exit", &json!({"exitCode": exit_code}))?;
            return Ok(());
        }
        let mut watch = crate::management::listeners::RevocationWatch::for_stream(stream)?;
        let mut last_write = std::time::Instant::now();
        loop {
            if watch.disconnected()? { return Ok(()); }
            if let Some(token_id) = watch.revoked() {
                crate::management::listeners::write_revoked_frame(stream, &token_id)?;
                return Ok(());
            }
            match receiver.recv_timeout(crate::management::listeners::STREAM_TICK) {
                Ok(TerminalEvent::Output(end, bytes)) => {
                    write_output(stream, end, &engine.encode(bytes.as_slice()), false, false)?;
                    last_write = std::time::Instant::now();
                }
                Ok(TerminalEvent::Exit(code)) => {
                    super::workspace_fs::write_sse(stream, "exit", &json!({"exitCode": code}))?;
                    return Ok(());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if last_write.elapsed() >= SSE_KEEPALIVE {
                        stream.write_all(b": keepalive\n\n")?;
                        stream.flush()?;
                        last_write = std::time::Instant::now();
                    }
                }
                // Dropped for falling behind (or terminal forgotten): the
                // client reconnects with its last offset.
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    })();
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Ok(())
}

fn write_output(
    stream: &mut TcpStream,
    offset: u64,
    data: &str,
    replay: bool,
    reset: bool,
) -> anyhow::Result<()> {
    let mut payload = json!({ "data": data });
    if replay {
        payload["replay"] = Value::Bool(true);
    }
    if reset {
        payload["reset"] = Value::Bool(true);
    }
    write!(
        stream,
        "id: {offset}\nevent: output\ndata: {}\n\n",
        serde_json::to_string(&payload)?
    )?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::workspace::Scope;
    use super::super::workspace::ScopeKind;
    use super::*;
    use std::time::Instant;

    fn state_with(bytes: &[u8], end: u64) -> TerminalState {
        TerminalState {
            scrollback: bytes.iter().copied().collect(),
            end_offset: end,
            exited: false,
            exit_code: None,
            subscribers: Vec::new(),
            cols: 80,
            rows: 24,
        }
    }

    #[test]
    fn replay_resumes_from_offset_or_resets() {
        let state = state_with(b"abcdef", 106);
        assert_eq!(replay_from(&state, Some(103)), (b"def".to_vec(), false));
        assert_eq!(replay_from(&state, Some(106)), (Vec::new(), false));
        assert_eq!(replay_from(&state, Some(50)), (b"abcdef".to_vec(), true));
        assert_eq!(replay_from(&state, None), (b"abcdef".to_vec(), true));
    }

    #[test]
    fn validates_requests() {
        assert!(validate_size(80, 24).is_ok());
        assert!(validate_size(0, 24).is_err());
        assert!(parse_body::<CreateRequest>(br#"{"cols":80,"rows":24,"extra":1}"#).is_err());
        assert_eq!(
            get("term-missing").err().unwrap().code,
            "terminal_not_found"
        );
    }

    fn collect_output(terminal: &Terminal, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let text = {
                let state = terminal.state.lock().unwrap();
                String::from_utf8_lossy(&state.scrollback.iter().copied().collect::<Vec<_>>())
                    .into_owned()
            };
            if text.contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    #[test]
    fn spawns_shell_echoes_input_resizes_and_kills() {
        let dir =
            std::env::temp_dir().join(format!("cutex-term-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let root = ResolvedRoot::from_dir(
            Scope {
                kind: ScopeKind::Session,
                id: "s-term".into(),
            },
            &dir,
        )
        .unwrap();
        let created = create(&root, br#"{"cols":100,"rows":30}"#).unwrap();
        let id = created["terminalId"].as_str().unwrap().to_string();
        assert_eq!(created["cols"], 100);
        let terminal = get(&id).unwrap();
        // ConPTY opens with a cursor-position request (ESC[6n) and blocks
        // until the terminal answers; xterm.js does this, the test must too.
        if cfg!(windows) && collect_output(&terminal, "\u{1b}[6n", Duration::from_secs(10)) {
            input(&id, br#"{"data":"\u001b[1;1R"}"#).unwrap();
        }
        // The marker appears in the shell's echo and in the command output.
        input(&id, br#"{"data":"echo cutexmarker42\r"}"#).unwrap();
        let found = collect_output(&terminal, "cutexmarker42", Duration::from_secs(20));
        let (dump, exited) = {
            let state = terminal.state.lock().unwrap();
            let bytes: Vec<u8> = state.scrollback.iter().copied().collect();
            (String::from_utf8_lossy(&bytes).into_owned(), state.exited)
        };
        assert!(
            found,
            "shell output should contain the marker; got {dump:?} (exited {exited})"
        );
        let resized = resize(&id, br#"{"cols":120,"rows":40}"#).unwrap();
        assert_eq!(resized["cols"], 120);
        let listed = list_resource_for(&root);
        assert_eq!(listed["terminals"].as_array().unwrap().len(), 1);
        let killed = kill(&id).unwrap();
        assert_eq!(killed["killed"], true);
        assert_eq!(get(&id).err().unwrap().code, "terminal_not_found");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !terminal.state.lock().unwrap().exited {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            terminal.state.lock().unwrap().exited,
            "terminal exits after kill"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    fn session_root(tag: &str) -> (std::path::PathBuf, ResolvedRoot) {
        let dir = std::env::temp_dir().join(format!(
            "cutex-term-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let root = ResolvedRoot::from_dir(
            Scope {
                kind: ScopeKind::Session,
                id: format!("s-{tag}"),
            },
            &dir,
        )
        .unwrap();
        (dir, root)
    }

    #[cfg(unix)]
    fn wait_exited(terminal: &Terminal, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if terminal.state.lock().unwrap().exited {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        terminal.state.lock().unwrap().exited
    }

    /// A shell that survives the hangup (here: ignores it; an interactive
    /// shell can also miss one during startup) is killed with its group.
    #[cfg(unix)]
    #[test]
    fn kill_escalates_when_the_shell_survives_the_hangup() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, root) = session_root("hup");
        let script = dir.join("ignore-hup.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\ntrap '' HUP\necho cutexready\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let body = json!({ "cols": 80, "rows": 24, "shell": script.to_string_lossy() }).to_string();
        let created = create(&root, body.as_bytes()).unwrap();
        let id = created["terminalId"].as_str().unwrap().to_string();
        let terminal = get(&id).unwrap();
        let pid = terminal.pid.unwrap() as libc::pid_t;
        assert!(collect_output(
            &terminal,
            "cutexready",
            Duration::from_secs(20)
        ));
        let started = Instant::now();
        assert_eq!(kill(&id).unwrap()["killed"], true);
        assert!(
            wait_exited(&terminal, Duration::from_secs(10)),
            "terminal exits after kill"
        );
        assert!(
            started.elapsed() >= KILL_GRACE,
            "the hangup was ignored, so the kill came after the grace"
        );
        // Reaped and gone, its `sleep` children (same process group) with it.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(unsafe { libc::killpg(pid, 0) }, -1);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Another process holding the pty slave (a leaked fd) cannot keep a
    /// killed terminal from exiting. Linux: the slave path comes from
    /// `/proc/<pid>/fd/0`.
    #[cfg(target_os = "linux")]
    #[test]
    fn exits_while_another_holder_keeps_the_pty_slave_open() {
        use std::os::unix::fs::OpenOptionsExt;
        let (dir, root) = session_root("slave");
        let created = create(&root, br#"{"cols":80,"rows":24,"shell":"/bin/sh"}"#).unwrap();
        let id = created["terminalId"].as_str().unwrap().to_string();
        let terminal = get(&id).unwrap();
        input(&id, br#"{"data":"echo cutex$((40+2))ok\r"}"#).unwrap();
        assert!(collect_output(
            &terminal,
            "cutex42ok",
            Duration::from_secs(20)
        ));
        let pid = terminal.pid.unwrap();
        let pts = std::fs::read_link(format!("/proc/{pid}/fd/0")).unwrap();
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(&pts)
            .unwrap();
        assert_eq!(kill(&id).unwrap()["killed"], true);
        assert!(
            wait_exited(&terminal, Duration::from_secs(10)),
            "terminal exits although {pts:?} is still open elsewhere"
        );
        drop(holder);
        let _ = std::fs::remove_dir_all(dir);
    }
}
