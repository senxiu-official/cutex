//! Browser script kernels (design 059 S2). The agent's JavaScript runs in a
//! Node process on this host, one per agent session, started on its first
//! `browser_script` call. The kernel holds no token and reaches nothing but
//! this broker: each browser step it asks for becomes an ordinary wire
//! command of the `browser` façade, sent back through Management with the
//! calling agent's own credential, so identity, active turn, approval,
//! leases and per-step idempotency apply to every step exactly as they do
//! to a direct tool call. Steps are admitted only while the script call
//! that started them is running. Variables live in the kernel process; an
//! idle kernel is reclaimed (pages and leases are not touched).
use super::{auth, model::*};
use crate::{
    http::server::SimpleHttpRequest,
    management::{
        server::ManagementRequestContext,
        v2::request_idempotency::{BeginRequest, RequestClaim},
    },
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{mpsc, Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

const KERNEL_SOURCE: &str = include_str!("script_kernel.cjs");
/// Wall time of one `browser_script` call. Each step keeps its own 10 s
/// command deadline inside it; the façade waits a little longer than this.
pub const SCRIPT_BUDGET: Duration = Duration::from_secs(45);
/// A kernel whose agent's turn ended this long ago is reclaimed.
pub const IDLE_AFTER_TURN: Duration = Duration::from_secs(60);
/// Any kernel unused this long is reclaimed (a turn may never report its end).
pub const MAX_IDLE: Duration = Duration::from_secs(15 * 60);
/// Kernels on this host at once; the least recently used idle one makes room.
pub const MAX_KERNELS: usize = 8;
const MAX_CODE_BYTES: usize = 64 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;
const MAX_STEPS: usize = 200;
const START_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScriptRequest {
    request_id: String,
    title: Option<String>,
    code: Option<String>,
    reset: Option<bool>,
}

struct Kernel {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    epoch: u64,
    runs: u64,
    last_used: Instant,
    busy: bool,
}
impl Drop for Kernel {
    fn drop(&mut self) {
        // Our own child, by handle: never a process tree or a name match.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct Registry {
    kernels: HashMap<String, Arc<KernelSlot>>,
    epochs: HashMap<String, u64>,
    reaper: bool,
}
struct KernelSlot {
    epoch: u64,
    ready: OnceLock<std::result::Result<Arc<Mutex<Kernel>>, &'static str>>,
    turn_ended: Mutex<Option<Instant>>,
}
impl KernelSlot {
    fn kernel(&self) -> Option<&Arc<Mutex<Kernel>>> {
        self.ready.get().and_then(|result| result.as_ref().ok())
    }
}
fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// The trusted end of an agent turn: the kernel stops being kept warm.
pub fn turn_completed(session: &str) {
    let slot = registry().lock().ok().and_then(|registry| registry.kernels.get(session).cloned());
    if let Some(slot) = slot {
        *slot.turn_ended.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }
}

/// Reclaim idle kernels. Busy kernels (a script call is running) are kept.
fn reap(now: Instant) {
    let Ok(mut registry) = registry().lock() else { return; };
    let expired: Vec<_> = registry.kernels.iter().filter_map(|(session, slot)| {
        let kernel = slot.kernel()?;
        if Arc::strong_count(slot) != 1 || Arc::strong_count(kernel) != 1 { return None; }
        let kernel = kernel.try_lock().ok()?;
        let ended = *slot.turn_ended.lock().unwrap_or_else(|e| e.into_inner());
        (!kernel.busy && (ended.is_some_and(|at| now.saturating_duration_since(at) >= IDLE_AFTER_TURN)
            || now.saturating_duration_since(kernel.last_used) >= MAX_IDLE)).then(|| session.clone())
    }).collect();
    let retired: Vec<_> = expired.into_iter().filter_map(|session| registry.kernels.remove(&session)).collect();
    drop(registry);
    drop(retired); // Child kill/wait never holds the host registry.
}

fn start_reaper(registry: &mut Registry) {
    if registry.reaper { return; }
    registry.reaper = true;
    std::thread::Builder::new()
        .name("browser-script-reaper".into())
        .spawn(|| loop {
            std::thread::sleep(Duration::from_secs(10));
            reap(Instant::now());
        })
        .ok();
}

/// The Node used for kernels: `CUTEX_BROWSER_SCRIPT_NODE`, else the Node the
/// DSH / Kimi runtime components already use. Same version rule as theirs.
pub fn node() -> Option<PathBuf> {
    static NODE: OnceLock<Mutex<Option<(Instant, Option<PathBuf>)>>> = OnceLock::new();
    let cache = NODE.get_or_init(Default::default);
    if let Some((at, found)) = cache.lock().ok()?.clone() {
        if at.elapsed() < Duration::from_secs(60) { return found; }
    }
    let found = node_candidates().into_iter().find(|path| {
        path.is_absolute() && path.is_file()
            && crate::components::dsh_runtime::node_version(path).is_ok_and(|v| crate::components::dsh_runtime::node_version_supported(&v))
    });
    if let Ok(mut cache) = cache.lock() { *cache = Some((Instant::now(), found.clone())); }
    found
}

fn node_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("CUTEX_BROWSER_SCRIPT_NODE") {
        candidates.push(PathBuf::from(path));
    }
    candidates.extend(crate::components::dsh_runtime::selected_node());
    candidates.extend(crate::components::kimi_runtime::selected_node());
    candidates
}

fn kernel_file() -> anyhow::Result<PathBuf> {
    let digest = format!("{:x}", Sha256::digest(KERNEL_SOURCE.as_bytes()));
    let dir = crate::config::paths::runtime_dir()?.join("browser/script");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("kernel-{}.cjs", &digest[..16]));
    if std::fs::read(&path).ok().as_deref() != Some(KERNEL_SOURCE.as_bytes()) {
        let temporary = dir.join(format!(".kernel-{}.{}.tmp", &digest[..16], std::process::id()));
        std::fs::write(&temporary, KERNEL_SOURCE)?;
        std::fs::rename(&temporary, &path)?;
    }
    Ok(path)
}

fn spawn(node: &Path, epoch: u64) -> anyhow::Result<Kernel> {
    let file = kernel_file()?;
    let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut command = Command::new(node);
    command.arg("--max-old-space-size=256").arg(&file)
        .current_dir(&dir)
        // No daemon environment: no tokens, no Management URL, no proxies.
        .env_clear()
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    for key in ["SystemRoot", "SYSTEMROOT", "WINDIR", "TEMP", "TMP", "TMPDIR", "LANG"] {
        if let Some(value) = std::env::var_os(key) { command.env(key, value); }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn()?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("kernel stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("kernel stdout"))?;
    let (sender, lines) = mpsc::channel();
    std::thread::Builder::new().name("browser-script-kernel".into()).spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = Vec::new();
            match reader.by_ref().take(MAX_LINE_BYTES as u64 + 1).read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if line.len() > MAX_LINE_BYTES => break,
                Ok(_) => {
                    if sender.send(String::from_utf8_lossy(&line).into_owned()).is_err() { break; }
                }
            }
        }
    })?;
    let kernel = Kernel { child, stdin, lines, epoch, runs: 0, last_used: Instant::now(), busy: false };
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        let left = deadline.checked_duration_since(Instant::now()).ok_or_else(|| anyhow::anyhow!("kernel start timeout"))?;
        let line = kernel.lines.recv_timeout(left)?;
        if serde_json::from_str::<Value>(&line).is_ok_and(|v| v["t"] == "ready") { return Ok(kernel); }
    }
}

/// This session's kernel, started if needed; `fresh` = no earlier variables.
fn kernel_for(session: &str) -> Result<(Arc<Mutex<Kernel>>, bool, Arc<KernelSlot>)> {
    kernel_for_with(session, |epoch| {
        let node = node().ok_or(RelayError::new("script_runtime_unavailable"))?;
        spawn(&node, epoch).map_err(|_| RelayError::new("script_runtime_unavailable"))
    })
}

fn kernel_for_with(session: &str, start: impl FnOnce(u64) -> Result<Kernel>) -> Result<(Arc<Mutex<Kernel>>, bool, Arc<KernelSlot>)> {
    let (slot, fresh, retired) = {
        let mut registry = registry().lock().map_err(|_| RelayError::new("browser_unavailable"))?;
        if let Some(slot) = registry.kernels.get(session) {
            (slot.clone(), false, None)
        } else {
            let mut retired = None;
            if registry.kernels.len() >= MAX_KERNELS {
                let idle = registry.kernels.iter().filter_map(|(key, slot)| {
                    let kernel = slot.kernel()?;
                    if Arc::strong_count(slot) != 1 || Arc::strong_count(kernel) != 1 { return None; }
                    kernel.try_lock().ok().filter(|k| !k.busy).map(|k| (key.clone(), k.last_used))
                }).min_by_key(|(_, used)| *used).map(|(key, _)| key);
                let Some(idle) = idle else { return Err(RelayError::new("busy")); };
                retired = registry.kernels.remove(&idle);
            }
            let epoch = registry.epochs.get(session).copied().unwrap_or(0) + 1;
            registry.epochs.insert(session.to_string(), epoch);
            let slot = Arc::new(KernelSlot { epoch, ready: OnceLock::new(), turn_ended: Mutex::new(None) });
            registry.kernels.insert(session.to_string(), slot.clone());
            start_reaper(&mut registry);
            (slot, true, retired)
        }
    };
    drop(retired);
    // Same-session callers share startup; unrelated sessions never wait on it.
    let result = slot.ready.get_or_init(|| start(slot.epoch).map(|kernel| Arc::new(Mutex::new(kernel))).map_err(|error| error.code));
    let kernel = match result {
        Ok(kernel) => kernel.clone(),
        Err(code) => {
            detach_slot(session, &slot);
            return Err(RelayError::new(code));
        }
    };
    let current = registry().lock().map_err(|_| RelayError::new("browser_unavailable"))?
        .kernels.get(session).is_some_and(|known| Arc::ptr_eq(known, &slot));
    if !current { return Err(RelayError::new("busy")); }
    Ok((kernel, fresh, slot))
}

fn detach_slot(session: &str, slot: &Arc<KernelSlot>) {
    let retired = registry().lock().ok().and_then(|mut registry| {
        if registry.kernels.get(session).is_some_and(|known| Arc::ptr_eq(known, slot)) {
            registry.kernels.remove(session)
        } else { None }
    });
    drop(retired);
}

fn reset(session: &str) -> u64 {
    let (retired, epoch) = {
        let Ok(mut registry) = registry().lock() else { return 0; };
        (registry.kernels.remove(session), registry.epochs.get(session).copied().unwrap_or(0))
    };
    drop(retired);
    epoch
}

fn drop_kernel(session: &str, kernel: &Arc<Mutex<Kernel>>) {
    let slot = registry().lock().ok().and_then(|registry| registry.kernels.get(session).cloned())
        .filter(|slot| slot.kernel().is_some_and(|current| Arc::ptr_eq(current, kernel)));
    if let Some(slot) = slot { detach_slot(session, &slot); }
}

/// The agent's own transport identity, for the steps it runs.
struct Caller {
    token: String,
    runtime: crate::role_revision::RuntimeAgentId,
    fence: crate::agent_bus::mcp::CallerFence,
}

fn caller(request: &SimpleHttpRequest, agent: &auth::AgentContext) -> Result<Caller> {
    let denied = || RelayError::new("permission_denied");
    let token = request.headers.get("authorization").and_then(|v| v.strip_prefix("Bearer ")).ok_or_else(denied)?;
    Ok(Caller {
        token: token.to_string(),
        runtime: crate::role_revision::RuntimeAgentId::new(agent.runtime.clone()).map_err(|_| denied())?,
        fence: crate::agent_bus::mcp::CallerFence { thread_id: agent.thread.clone(), generation: agent.generation },
    })
}

/// `POST /v2/browser/scripts` (cxa1): run one script call, or reset.
pub fn submit(request: &SimpleHttpRequest, context: ManagementRequestContext) -> Result<super::agent_api::Reply> {
    let agent = auth::agent(request, context, false)?;
    if crate::agent_bus::tool_policy::browser_surface(&agent.session) != Some(crate::agent_bus::mcp_browser::Surface::Script) {
        return Err(RelayError::new("browser_disabled"));
    }
    let input: ScriptRequest = parse(&request.body)?;
    let reset_only = input.reset == Some(true);
    let valid = identifier(&input.request_id)
        && if reset_only { input.code.is_none() && input.title.is_none() } else {
            input.reset.is_none()
                && input.title.as_deref().is_some_and(|t| !t.trim().is_empty() && t.chars().count() <= 200 && !t.chars().any(char::is_control))
                && input.code.as_deref().is_some_and(|c| !c.trim().is_empty() && c.len() <= MAX_CODE_BYTES)
        };
    if !valid {
        return Err(RelayError::new("invalid_request"));
    }
    if !reset_only && !identifier(&agent.turn) {
        return Err(RelayError::new("permission_denied"));
    }
    // Live check: the owner may have switched the agent out of full access.
    if !super::owner::agent(context, &agent.session)?.full_access {
        return Err(RelayError::new("permission_denied"));
    }
    let repo = super::agent_api::repository()?;
    let scope = format!("{}script", super::agent_api::prefix(&agent));
    let canonical = json!({"tool":"browser_script","title":input.title,"codeSha256":input.code.as_deref().map(|c| format!("{:x}", Sha256::digest(c.as_bytes()))),"reset":reset_only});
    let claim = match repo.begin_for_method("browser/write", &scope, &input.request_id, &canonical)
        .map_err(|_| RelayError::new("browser_unavailable"))? {
        BeginRequest::Forward(claim) => claim,
        // A retransmit never runs the script again.
        BeginRequest::Completed(response) => return Ok(super::agent_api::Reply::text(json!({
            "requestId": input.request_id, "status": response.body["status"], "resultRetained": false,
            "note": "This script call already ran; its output is not kept. Observe the page before doing anything else."}))),
        BeginRequest::InProgress => return Err(RelayError::new("script_running")),
        BeginRequest::Conflict => return Err(RelayError::new("request_conflict")),
        BeginRequest::OutcomeUnknown => return Err(RelayError::new("outcome_unknown")),
        BeginRequest::CapacityExceeded(_) => return Err(RelayError::new("idempotency_capacity_exceeded")),
    };
    let settle = |status: &str, failed: bool| {
        let _ = repo.complete(&claim, if failed { 400 } else { 200 }, "Browser script result", json!({"status": status}));
    };
    if reset_only {
        let epoch = reset(&agent.session);
        settle("succeeded", false);
        return Ok(super::agent_api::Reply::text(json!({"reset": true, "previousEpoch": epoch,
            "note": "Kernel variables are gone. Pages, tabs and leases are unchanged; nothing was undone."})));
    }
    let caller = caller(request, &agent)?;
    // Steps belong to the turn that started this call: after it ends, or
    // while the owner pauses the agent, no further step is sent.
    let (session, turn) = (agent.session.clone(), agent.turn.clone());
    let outcome = run(&agent.session, &input.request_id, input.title.as_deref().unwrap_or_default(), input.code.as_deref().unwrap_or_default(),
        &mut |step, tool, args| match step_gate(context, &session, &turn) {
            Some(code) => refusal(code),
            None => crate::agent_bus::mcp_browser::send_wire(&caller.token, &caller.runtime, &caller.fence, step, tool, args),
        });
    match outcome {
        Ok(result) => {
            settle(if result.failed { "failed" } else { "succeeded" }, result.failed);
            Ok(super::agent_api::Reply::mcp(result.content))
        }
        Err(error) => {
            settle("outcome_unknown", true);
            let _: &RequestClaim = &claim;
            Err(error)
        }
    }
}

/// Why the next step must not be sent, if any.
fn step_gate(context: ManagementRequestContext, session: &str, turn: &str) -> Option<&'static str> {
    let current = (context.load_runtime_status)(session).ok().flatten().and_then(|status| status.active_turn_id);
    if current.as_deref() != Some(turn) {
        return Some("turn_ended");
    }
    match crate::asks::store::AskStore::open_default() {
        Ok(store) => pause_refusal(&store, session),
        Err(_) => Some("pause_state_unavailable"),
    }
}

/// How long a step waits while the host authority fence is held (another
/// agent's start or stop, an owner input's admission) before giving up.
const FENCE_WAIT: Duration = Duration::from_secs(5);

/// Only a recorded pause (the agent's own or its project's) is agent_paused.
/// A busy authority fence decides nothing: wait for it without blocking on it,
/// then say the host was busy. An unreadable pause state holds the step too,
/// under its own code; neither claims the owner paused the agent.
fn pause_refusal(store: &crate::asks::store::AskStore, session: &str) -> Option<&'static str> {
    pause_refusal_with(FENCE_WAIT, || crate::asks::pause_admission::gate(store, session))
}
fn pause_refusal_with(wait: Duration, mut gate: impl FnMut() -> anyhow::Result<crate::asks::pause_admission::Gate>) -> Option<&'static str> {
    use crate::asks::pause_admission::Gate;
    let deadline = Instant::now() + wait;
    loop {
        match gate() {
            Ok(Gate::Open) => return None,
            Ok(Gate::Paused) => return Some("agent_paused"),
            Ok(Gate::Deferred) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(Gate::Deferred) => return Some("host_busy"),
            Err(_) => return Some("pause_state_unavailable"),
        }
    }
}

/// Codes the broker answers itself: no step was sent, so no request id.
const UNSENT: &[&str] = &["turn_ended", "agent_paused", "host_busy", "pause_state_unavailable", "step_limit", "script_timeout", "invalid_request"];

fn refusal(code: &str) -> Value {
    let message = match code {
        "turn_ended" => "This turn has ended; no step was sent. Browser steps run only in the turn that started the script.",
        "host_busy" => "The host was busy for a few seconds (another agent starting or stopping, or an owner action); no step was sent. Send this step again.",
        "pause_state_unavailable" => "The host could not read whether this agent is paused; no step was sent. Try again shortly; tell the owner if it keeps happening.",
        _ => "The owner paused this agent; no step was sent. End this call; work continues when the owner resumes.",
    };
    json!({"isError": true, "content": [{"type": "text", "text": json!({"error": {"code": code, "message": message}}).to_string()}]})
}

pub struct RunResult {
    pub content: Value,
    pub failed: bool,
}

/// Run `code` in `session`'s kernel. `send` performs one wire command (as
/// the agent). Public for tests with a fake wire.
pub fn run(session: &str, request: &str, title: &str, code: &str, send: &mut dyn FnMut(&str, &str, &Value) -> Value) -> Result<RunResult> {
    let (kernel, fresh, slot) = kernel_for(session)?;
    let deadline = Instant::now() + SCRIPT_BUDGET;
    let mut guard = loop {
        match kernel.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::WouldBlock) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return Err(RelayError::new("busy")),
        }
    };
    guard.busy = true;
    guard.runs += 1;
    *slot.turn_ended.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let run_id = guard.runs;
    let epoch = guard.epoch;
    let started = json!({"t":"run","run":run_id,"title":title,"code":code,"syncBudgetMs":SCRIPT_BUDGET.as_millis() as u64});
    let mut steps: Vec<Value> = Vec::new();
    let mut images: Vec<Value> = Vec::new();
    let outcome = (|| -> std::result::Result<Value, &'static str> {
        writeln!(guard.stdin, "{started}").and_then(|_| guard.stdin.flush()).map_err(|_| "kernel_lost")?;
        loop {
            let left = deadline.checked_duration_since(Instant::now()).ok_or("script_timeout")?;
            let line = match guard.lines.recv_timeout(left) {
                Ok(line) => line,
                Err(mpsc::RecvTimeoutError::Timeout) => return Err("script_timeout"),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err("kernel_lost"),
            };
            let Ok(message) = serde_json::from_str::<Value>(&line) else { continue; };
            if message["run"] != json!(run_id) { continue; }
            match message["t"].as_str() {
                Some("call") => {
                    let reply = step(request, &message, &mut steps, &mut images, deadline, send);
                    writeln!(guard.stdin, "{reply}").and_then(|_| guard.stdin.flush()).map_err(|_| "kernel_lost")?;
                }
                Some("done") => return Ok(message),
                _ => {}
            }
        }
    })();
    guard.busy = false;
    guard.last_used = Instant::now();
    let kernel_view = json!({"epoch": epoch, "fresh": fresh});
    let done = match outcome {
        Ok(done) => done,
        Err(code) => {
            // A script past its budget (or a dead kernel) loses the kernel:
            // its variables are gone; steps already sent keep their own state.
            drop(guard);
            drop_kernel(session, &kernel);
            let body = json!({"ok": false, "error": {"code": code, "message": if code == "script_timeout" {
                "The script ran past its time budget; the kernel was stopped and its variables are gone. Steps it already started keep their own results: check them with browser.status(requestId)."
            } else { "The script kernel stopped; its variables are gone." }}, "steps": steps, "kernel": kernel_view});
            return Ok(RunResult { content: text_content(&body), failed: true });
        }
    };
    let ok = done["ok"] == true;
    let mut text = String::new();
    let mut emitted_image: Option<Value> = None;
    let mut notes = Vec::new();
    for item in done["output"].as_array().into_iter().flatten() {
        match item["type"].as_str() {
            Some("text") => {
                if !text.is_empty() && !text.ends_with('\n') { text.push('\n'); }
                text.push_str(item["text"].as_str().unwrap_or_default());
            }
            Some("image") => {
                let found = item["handle"].as_str().and_then(|h| images.iter().find(|i| i["handle"] == h));
                match (found, &emitted_image) {
                    (Some(image), None) => emitted_image = Some(image["block"].clone()),
                    (Some(_), Some(_)) => notes.push("only the first emitted image is returned; emit others in later calls"),
                    (None, _) => notes.push("emitImage needs a screenshot result from this call"),
                }
            }
            _ => {}
        }
    }
    let mut body = json!({"ok": ok, "output": text, "steps": steps, "kernel": kernel_view});
    if done["truncated"] == true { body["truncated"] = json!(true); }
    if !ok { body["error"] = done["error"].clone(); }
    if !notes.is_empty() { body["notes"] = json!(notes); }
    let mut content = text_content(&body);
    // Keep the text envelope inside the façade's text budget.
    if serde_json::to_vec(&content).map_or(0, |v| v.len()) > crate::agent_bus::mcp_browser::MAX_TEXT_BYTES - 512 {
        body["output"] = json!(cut(&text, 32 * 1024));
        body["truncated"] = json!(true);
        if serde_json::to_vec(&body).map_or(0, |v| v.len()) > 48 * 1024 { body["steps"] = json!(steps.len()); }
        content = text_content(&body);
    }
    if let Some(image) = emitted_image {
        content["content"].as_array_mut().unwrap().push(image);
    }
    Ok(RunResult { content, failed: !ok })
}

fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max { return text; }
    let mut end = max;
    while !text.is_char_boundary(end) { end -= 1; }
    &text[..end]
}

fn text_content(body: &Value) -> Value {
    json!({"content":[{"type":"text","text":body.to_string()}]})
}

/// One kernel step → one `browser` command under a derived request id.
fn step(request: &str, message: &Value, steps: &mut Vec<Value>, images: &mut Vec<Value>, deadline: Instant,
    send: &mut dyn FnMut(&str, &str, &Value) -> Value) -> Value {
    let (run, seq) = (message["run"].clone(), message["seq"].clone());
    let reply_error = |error: Value| json!({"t":"reply","run":run,"seq":seq,"error":error});
    let command = message["command"].as_str().unwrap_or_default().to_string();
    if steps.len() >= MAX_STEPS {
        return reply_error(json!({"code":"step_limit","message":"This script call reached its step limit; continue in another call."}));
    }
    if Instant::now() >= deadline {
        return reply_error(json!({"code":"script_timeout","message":"The script's time budget is used up; no step was sent."}));
    }
    let ordinal = steps.len() + 1;
    let step_id = crate::agent_bus::mcp_browser::step_request(request, &format!("script-step-{ordinal}"));
    let args = json!({"command": command, "args": message["args"].clone()});
    let result = crate::agent_bus::mcp_browser::invoke_with(&step_id, crate::agent_bus::mcp_browser::COMMAND, &args, |r, t, a| send(r, t, a));
    let failed = result["isError"] == true;
    let mut parsed = Vec::new();
    let mut image = None;
    for block in result["content"].as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => {
                let text = block["text"].as_str().unwrap_or_default();
                parsed.push(serde_json::from_str::<Value>(text).unwrap_or_else(|_| json!(text)));
            }
            Some("image") => {
                let handle = format!("img-{}", images.len() + 1);
                images.push(json!({"handle": handle, "block": block}));
                image = Some(json!({"image": handle, "mimeType": block["mimeType"]}));
            }
            _ => {}
        }
    }
    let error_code = parsed.first().and_then(|v| v.pointer("/error/code")).and_then(Value::as_str).map(str::to_string);
    let mut record = json!({"step": ordinal, "command": command, "requestId": step_id, "ok": !failed});
    // The owner's timeline names act steps by kind (never their values).
    if let Some(kind) = message.pointer("/args/action/kind").and_then(Value::as_str)
        .filter(|kind| command == "act" && (1..=32).contains(&kind.len()) && kind.chars().all(|c| c.is_ascii_alphabetic())) {
        record["kind"] = json!(kind);
    }
    if let Some(code) = &error_code {
        record["error"] = json!(code);
        if UNSENT.contains(&code.as_str()) { record.as_object_mut().unwrap().remove("requestId"); }
    }
    steps.push(record);
    if failed {
        let mut error = parsed.into_iter().next().and_then(|v| v.get("error").cloned()).unwrap_or_else(|| json!({"code":"browser_unavailable"}));
        if error["code"].as_str().is_some_and(|code| UNSENT.contains(&code)) {
            // Nothing was sent: no request id to look up.
            if let Some(error) = error.as_object_mut() { error.remove("requestId"); }
        } else if error.get("requestId").is_none() {
            error["requestId"] = json!(step_id);
        }
        return reply_error(error);
    }
    let value = match (parsed.len(), image) {
        (_, Some(image)) => image,
        (1, None) => parsed.remove(0),
        _ => json!(parsed),
    };
    json!({"t":"reply","run":run,"seq":seq,"result":value})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_startup_does_not_block_other_sessions_or_turn_bookkeeping() {
        let session = format!("startup-stalled-{}", uuid::Uuid::new_v4());
        let (began, ready) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let slow_session = session.clone();
        let slow = std::thread::spawn(move || kernel_for_with(&slow_session, |_| {
            began.send(()).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(5));
            Err(RelayError::new("script_runtime_unavailable"))
        }).err().unwrap().code);
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let (done, completed) = mpsc::sync_channel(1);
        let fast_session = session.clone();
        let fast = std::thread::spawn(move || {
            turn_completed(&fast_session);
            let other = format!("startup-other-{}", uuid::Uuid::new_v4());
            assert_eq!(kernel_for_with(&other, |_| Err(RelayError::new("script_runtime_unavailable"))).err().unwrap().code, "script_runtime_unavailable");
            reset(&other);
            done.send(()).unwrap();
        });
        let result = completed.recv_timeout(Duration::from_secs(1));
        release.send(()).unwrap();
        assert_eq!(slow.join().unwrap(), "script_runtime_unavailable");
        fast.join().unwrap();
        assert!(result.is_ok(), "startup held the global kernel registry");
        assert!(!registry().lock().unwrap().kernels.contains_key(&session));
    }

    #[cfg(unix)]
    fn fake_kernel(epoch: u64) -> Result<Kernel> {
        let mut child = Command::new("/bin/sh").args(["-c", "exec cat"]).stdin(Stdio::piped()).stdout(Stdio::null()).spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let (_sender, lines) = mpsc::channel();
        Ok(Kernel { child, stdin, lines, epoch, runs: 0, last_used: Instant::now(), busy: false })
    }

    #[cfg(unix)]
    #[test]
    fn simultaneous_same_session_callers_share_one_startup() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let session = format!("startup-shared-{}", uuid::Uuid::new_v4());
        let starts = Arc::new(AtomicUsize::new(0));
        let (began, ready) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let (first_session, first_starts) = (session.clone(), starts.clone());
        let first = std::thread::spawn(move || kernel_for_with(&first_session, |epoch| {
            first_starts.fetch_add(1, Ordering::SeqCst);
            began.send(()).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(5));
            fake_kernel(epoch)
        }));
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let (second_session, second_starts) = (session.clone(), starts.clone());
        let second = std::thread::spawn(move || kernel_for_with(&second_session, |epoch| {
            second_starts.fetch_add(1, Ordering::SeqCst);
            fake_kernel(epoch)
        }));
        let deadline = Instant::now() + Duration::from_secs(2);
        while Arc::strong_count(registry().lock().unwrap().kernels.get(&session).unwrap()) < 3 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        release.send(()).unwrap();
        let (a, _, a_slot) = first.join().unwrap().unwrap();
        let (b, _, b_slot) = second.join().unwrap().unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        reset(&session);
        drop((a, b, a_slot, b_slot));
    }

    #[cfg(unix)]
    #[test]
    fn reset_during_startup_cannot_replace_a_new_epoch() {
        let session = format!("startup-reset-{}", uuid::Uuid::new_v4());
        let (began, ready) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let slow_session = session.clone();
        let slow = std::thread::spawn(move || kernel_for_with(&slow_session, |epoch| {
            began.send(()).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(5));
            fake_kernel(epoch)
        }).err().unwrap().code);
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(reset(&session), 1);
        let (new_kernel, fresh, new_slot) = kernel_for_with(&session, fake_kernel).unwrap();
        assert!(fresh);
        assert_eq!(new_kernel.lock().unwrap().epoch, 2);
        release.send(()).unwrap();
        assert_eq!(slow.join().unwrap(), "busy");
        assert!(Arc::ptr_eq(registry().lock().unwrap().kernels.get(&session).unwrap(), &new_slot));
        reset(&session);
        drop(new_slot);
        drop(new_kernel);
    }

    fn node_or_skip() -> bool {
        if node().is_some() { return true; }
        eprintln!("no supported Node (CUTEX_BROWSER_SCRIPT_NODE / DSH / Kimi): kernel tests skipped");
        false
    }
    fn body(result: &RunResult) -> Value {
        serde_json::from_str(result.content["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// Another agent's start or an owner admission holding the host authority
    /// fence is not a pause: the step is not refused as agent_paused.
    #[test]
    fn a_busy_authority_fence_is_not_reported_as_an_owner_pause() {
        let (store, dir) = crate::asks::store::tests::temp_store("script-step-fence");
        store.with_state(|_| ()).unwrap();
        let session = "cutex.script-fence";
        assert_eq!(pause_refusal(&store, session), None);
        let roster = crate::agent_management::AgentManagementStore::open_default().unwrap();
        let fence = roster.lock_mutations().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let held = crate::asks::store::AskStore::at(&dir);
        let worker = std::thread::spawn(move || tx.send(pause_refusal(&held, session)).unwrap());
        let refused = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        worker.join().unwrap();
        assert_ne!(refused, Some("agent_paused"), "a busy fence must not read as the owner's pause");
        // Released within the step's wait: the step goes ahead.
        let (tx, rx) = std::sync::mpsc::channel();
        let held = crate::asks::store::AskStore::at(&dir);
        let worker = std::thread::spawn(move || tx.send(pause_refusal(&held, session)).unwrap());
        std::thread::sleep(Duration::from_millis(300));
        drop(fence);
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), None);
        worker.join().unwrap();
        // The owner's real pause still refuses the step as agent_paused.
        let _ = store.with_state(|s| crate::asks::agent_pause::pause(s, session, "owner", None, chrono::Utc::now())).unwrap();
        assert_eq!(pause_refusal(&store, session), Some("agent_paused"));
        let _ = std::fs::remove_dir_all(dir);
    }
    /// The step's decision over the pause gate: wait out a busy fence, then
    /// say the host was busy; an unreadable state is its own code.
    #[test]
    fn step_gate_waits_out_a_busy_fence_and_names_each_refusal() {
        use crate::asks::pause_admission::Gate;
        let mut answers = vec![Gate::Deferred, Gate::Deferred, Gate::Open].into_iter();
        assert_eq!(pause_refusal_with(Duration::from_secs(5), || Ok(answers.next().unwrap())), None);
        let started = Instant::now();
        assert_eq!(pause_refusal_with(Duration::from_millis(200), || Ok(Gate::Deferred)), Some("host_busy"));
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert_eq!(pause_refusal_with(Duration::from_secs(5), || Ok(Gate::Paused)), Some("agent_paused"));
        assert_eq!(pause_refusal_with(Duration::from_secs(5), || anyhow::bail!("store")), Some("pause_state_unavailable"));
        for code in ["host_busy", "pause_state_unavailable"] {
            assert!(UNSENT.contains(&code));
            assert!(!refusal(code).to_string().contains("owner paused"), "{code}");
        }
        assert!(refusal("host_busy").to_string().contains("Send this step again"));
    }
    /// The kernel keeps state between calls; every step is one `browser`
    /// command with its own derived request id; failures carry it; images
    /// are returned only when emitted; reset forgets variables.
    #[test]
    fn paused_or_ended_turn_refuses_each_step_without_a_request_id() {
        if !node_or_skip() { return; }
        let session = format!("cutex.script-{}", uuid::Uuid::new_v4());
        let mut sent = 0;
        let mut gate = |_: &str, _: &str, _: &Value| -> Value { sent += 1; refusal(if sent == 1 { "agent_paused" } else { "turn_ended" }) };
        let result = run(&session, "browser-outer-p", "Pay", r#"
            const seen = [];
            for (const url of ["https://example.com/a", "https://example.com/b"]) {
              try { await browser.open(url); } catch (e) { seen.push([e.code, e.requestId ?? null]); }
            }
            return seen;"#, &mut gate).unwrap();
        let b = body(&result);
        assert_eq!(b["ok"], true, "{b}");
        let output: Value = serde_json::from_str(b["output"].as_str().unwrap()).unwrap();
        assert_eq!(output, json!([["agent_paused", null], ["turn_ended", null]]));
        assert!(b["steps"].as_array().unwrap().iter().all(|step| step.get("requestId").is_none()), "{b}");
        reset(&session);
    }
    #[test]
    fn script_kernel_runs_steps_through_the_browser_command_and_keeps_state() {
        if !node_or_skip() { return; }
        let session = format!("cutex.script-{}", uuid::Uuid::new_v4());
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jvN8AAAAASUVORK5CYII=";
        let calls = std::cell::RefCell::new(Vec::new());
        let mut wire = |request: &str, tool: &str, args: &Value| -> Value {
            calls.borrow_mut().push((request.to_string(), tool.to_string(), args.clone()));
            let text = |v: Value| json!({"content":[{"type":"text","text":v.to_string()}]});
            match tool {
                "browser_open" => text(json!({"kind":"tabs.open","tab":{"guestId":"g1","url":args["url"],"title":"T","pageGeneration":1}})),
                "browser_read" => text(json!({"kind":"tab.snapshot","nodes":[{"ref":"r1"},{"ref":"r2"}]})),
                "browser_screenshot" => json!({"content":[{"type":"image","mimeType":"image/png","data":png}]}),
                _ => crate::agent_bus::mcp_browser::failure("outcome_unknown"),
            }
        };
        let first = run(&session, "browser-outer-1", "Shop", r#"
            state.count = (state.count ?? 0) + 1;
            const tab = await browser.open("https://example.com/");
            const page = await tab.read();
            globalThis.guest = tab.guestId;
            return { count: state.count, refs: page.nodes.length };"#, &mut wire).unwrap();
        let b = body(&first);
        assert_eq!(b["ok"], true, "{b}");
        assert_eq!(b["kernel"]["fresh"], true);
        assert!(b["output"].as_str().unwrap().contains("\"refs\": 2"));
        assert_eq!(b["steps"].as_array().unwrap().len(), 2);
        {
            let calls = calls.borrow();
            assert_eq!(calls[0].1, "browser_open");
            assert_eq!(calls[0].2["title"], "Shop", "the call's title labels its steps");
            assert_eq!(calls[0].0, crate::agent_bus::mcp_browser::step_request("browser-outer-1", "script-step-1"));
            assert_ne!(calls[0].0, calls[1].0);
        }
        // Same kernel: state kept; a failed step throws BrowserError with its request id.
        let second = run(&session, "browser-outer-2", "Shop", r#"
            try { await browser.tab(guest).click("r1"); } catch (e) { emit({ code: e.code, id: e.requestId }); }
            const shot = await browser.tab(guest).screenshot();
            emitImage(shot);
            return state.count;"#, &mut wire).unwrap();
        let b = body(&second);
        assert_eq!(b["kernel"]["fresh"], false);
        assert!(b["output"].as_str().unwrap().contains("outcome_unknown"), "{b}");
        assert!(b["output"].as_str().unwrap().contains(&crate::agent_bus::mcp_browser::step_request("browser-outer-2", "script-step-1")));
        assert_eq!(b["steps"][0]["error"], "outcome_unknown");
        assert_eq!(b["steps"][0]["kind"], "click", "act steps carry their kind for the timeline");
        assert_eq!(second.content["content"][1]["type"], "image");
        assert!(!second.content.to_string().contains("img-1\"") || second.content["content"].as_array().unwrap().len() == 2);
        // Errors in the code itself are results, not transport failures.
        let third = run(&session, "browser-outer-3", "Shop", "throw new TypeError('boom')", &mut wire).unwrap();
        assert!(third.failed);
        assert_eq!(body(&third)["error"]["name"], "TypeError");
        // Reset: variables gone, new epoch.
        reset(&session);
        let fourth = run(&session, "browser-outer-4", "Shop", "return typeof guest", &mut wire).unwrap();
        let b = body(&fourth);
        assert_eq!(b["kernel"]["fresh"], true);
        assert_eq!(b["output"], "undefined");
        assert_eq!(b["kernel"]["epoch"], 2);
        // No step outside a running call: a step started after the call ends is refused in the kernel.
        let fifth = run(&session, "browser-outer-5", "Shop", "globalThis.late = browser.list().catch(e => e.code); return 'x'", &mut wire).unwrap();
        assert_eq!(body(&fifth)["ok"], true);
        let sixth = run(&session, "browser-outer-6", "Shop", "return await late", &mut wire).unwrap();
        let output = body(&sixth)["output"].clone();
        assert!(output == "script_not_running" || body(&sixth)["steps"].as_array().unwrap().len() <= 1, "{output}");
        reset(&session);
    }

    #[test]
    fn script_past_its_budget_loses_only_its_kernel() {
        if !node_or_skip() { return; }
        // Exercise the stop path with a CPU loop: the sync budget stops it.
        let session = format!("cutex.script-{}", uuid::Uuid::new_v4());
        let mut wire = |_: &str, _: &str, _: &Value| -> Value { panic!("no step expected") };
        let result = run(&session, "browser-loop", "Loop", "state.x = 1; return 1", &mut wire).unwrap();
        assert_eq!(body(&result)["ok"], true);
        assert!(registry().lock().unwrap().kernels.contains_key(&session));
        reset(&session);
        assert!(!registry().lock().unwrap().kernels.contains_key(&session));
    }

    #[test]
    fn idle_kernels_are_reaped_after_turn_end_or_long_idle() {
        if !node_or_skip() { return; }
        let session = format!("cutex.script-{}", uuid::Uuid::new_v4());
        let mut wire = |_: &str, _: &str, _: &Value| -> Value { panic!("no step expected") };
        run(&session, "browser-idle", "Idle", "return 1", &mut wire).unwrap();
        reap(Instant::now());
        assert!(registry().lock().unwrap().kernels.contains_key(&session), "a running turn keeps its kernel");
        turn_completed(&session);
        reap(Instant::now() + IDLE_AFTER_TURN + Duration::from_secs(1));
        assert!(!registry().lock().unwrap().kernels.contains_key(&session), "reclaimed after turn end");
    }
}
