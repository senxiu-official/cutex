//! The script sandbox (design §2.2): QuickJS (rquickjs) in a child process
//! `cutex workflow exec`, one per running run.
//!
//! The child knows nothing but the script: no token, no store, no files, no
//! network, no timers. It speaks JSON lines on stdio:
//!
//! parent → child
//! - `{"t":"start","body":…,"args":<json|null>,"budgetTotal":N,"spent":N}` (first line)
//! - `{"t":"result","id":N,"ok":true,"value":…,"spent":N}`
//! - `{"t":"result","id":N,"ok":false,"error":{"code","message"},"spent":N}`
//! - `{"t":"spent","spent":N}`
//!
//! child → parent
//! - `{"t":"agent","id":N,"prompt":…,"opts":{…}}`
//! - `{"t":"log","text":…,"level":"info"|"warn"}`
//! - `{"t":"phase","title":…}`
//! - `{"t":"done","ok":true,"value":…}` / `{"t":"done","ok":false,"error":{"code","message"}}`
//!
//! Determinism: `Date.now()`, an argless `new Date()` / `Date()` and
//! `Math.random()` throw. Limits: heap 64 MiB, stack 1 MiB, 5 s of
//! uninterrupted synchronous CPU per slice (`script_cpu_limit`).

use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use rquickjs::promise::PromiseState;
use rquickjs::{CatchResultExt, Context, Function, Persistent, Promise, Runtime, Value};
use serde_json::{json, Value as Json};

pub const SCRIPT_FILE: &str = "workflow.js";
pub const HEAP_LIMIT_BYTES: usize = 64 << 20;
pub const STACK_LIMIT_BYTES: usize = 1 << 20;
pub const CPU_SLICE: Duration = Duration::from_secs(5);
pub const MAX_ITEMS: usize = 4096;

const PRELUDE: &str = include_str!("prelude.js");

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A runtime with the sandbox limits and a CPU-slice watchdog.
struct Sandbox {
    runtime: Runtime,
    context: Context,
    deadline: Arc<AtomicU64>,
    tripped: Arc<AtomicBool>,
}

impl Sandbox {
    fn new() -> anyhow::Result<Self> {
        let runtime = Runtime::new().context("QuickJS runtime")?;
        runtime.set_memory_limit(HEAP_LIMIT_BYTES);
        runtime.set_max_stack_size(STACK_LIMIT_BYTES);
        let deadline = Arc::new(AtomicU64::new(0));
        let tripped = Arc::new(AtomicBool::new(false));
        let (d, t) = (deadline.clone(), tripped.clone());
        runtime.set_interrupt_handler(Some(Box::new(move || {
            let limit = d.load(Ordering::Relaxed);
            if limit != 0 && now_ms() > limit {
                t.store(true, Ordering::Relaxed);
                return true;
            }
            false
        })));
        let context = Context::full(&runtime).context("QuickJS context")?;
        Ok(Self { runtime, context, deadline, tripped })
    }

    fn arm(&self) {
        self.deadline.store(now_ms() + CPU_SLICE.as_millis() as u64, Ordering::Relaxed);
    }

    fn disarm(&self) {
        self.deadline.store(0, Ordering::Relaxed);
    }

    /// Run queued promise jobs until none are left (one CPU slice).
    fn pump(&self) -> Result<(), String> {
        self.arm();
        loop {
            if !self.runtime.is_job_pending() {
                break;
            }
            if let Err(error) = self.runtime.execute_pending_job() {
                // A job threw an uncaught error outside any promise chain.
                let message = error.0.with(|ctx| ctx.catch().as_exception().map(|e| e.to_string()).unwrap_or_else(|| "uncaught error".into()));
                self.disarm();
                return Err(message);
            }
            if self.tripped.load(Ordering::Relaxed) {
                break;
            }
        }
        self.disarm();
        Ok(())
    }
}

fn exception_text(ctx: &rquickjs::Ctx<'_>, error: rquickjs::CaughtError<'_>) -> String {
    let _ = ctx;
    error.to_string()
}

/// Evaluate a (pre-checked) pure literal and return it as JSON.
pub fn eval_literal(literal: &str) -> Result<Json, String> {
    let sandbox = Sandbox::new().map_err(|e| format!("{e:#}"))?;
    sandbox.arm();
    let result = sandbox.context.with(|ctx| -> Result<Json, String> {
        let source = format!("JSON.stringify(({literal}))");
        let text: String = ctx.eval(source).catch(&ctx).map_err(|e| exception_text(&ctx, e))?;
        serde_json::from_str(&text).map_err(|e| e.to_string())
    });
    sandbox.disarm();
    result
}

/// Compile without running (syntax check). Returns the QuickJS message + stack.
pub fn compile_only(body: &str, file: &str) -> Result<(), String> {
    let sandbox = Sandbox::new().map_err(|e| format!("{e:#}"))?;
    sandbox.arm();
    let result = sandbox.context.with(|ctx| -> Result<(), String> {
        let mut options = rquickjs::context::EvalOptions::default();
        options.filename = Some(file.to_string());
        options.strict = true;
        let _: Value = ctx.eval_with_options(body.as_bytes().to_vec(), options).catch(&ctx).map_err(|e| e.to_string())?;
        Ok(())
    });
    sandbox.disarm();
    result
}

fn error_json(code: &str, message: impl Into<String>) -> Json {
    json!({"code": code, "message": message.into()})
}

/// The child's main loop. Reads the start line, runs the script, answers
/// agent results, writes `done` and returns. Generic over the streams so tests
/// can run it in-process.
pub fn run(mut input: impl BufRead, output: impl Write) -> anyhow::Result<()> {
    let output = Rc::new(RefCell::new(output));
    let emit = {
        let output = output.clone();
        move |value: &Json| -> anyhow::Result<()> {
            let mut out = output.borrow_mut();
            serde_json::to_writer(&mut *out, value)?;
            out.write_all(b"\n")?;
            out.flush()?;
            Ok(())
        }
    };
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        anyhow::bail!("workflow exec: no start line");
    }
    let start: Json = serde_json::from_str(&line).context("workflow exec: bad start line")?;
    anyhow::ensure!(start.get("t").and_then(Json::as_str) == Some("start"), "workflow exec: first line must be start");
    let body = start.get("body").and_then(Json::as_str).context("start.body")?.to_string();
    let args = start.get("args").cloned().unwrap_or(Json::Null);
    let budget_total = start.get("budgetTotal").and_then(Json::as_u64);
    let spent = start.get("spent").and_then(Json::as_u64).unwrap_or(0);

    let sandbox = Sandbox::new()?;
    let outbox: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let finish = |result: Result<Json, Json>| -> anyhow::Result<()> {
        match result {
            Ok(value) => emit(&json!({"t": "done", "ok": true, "value": value})),
            Err(error) => emit(&json!({"t": "done", "ok": false, "error": error})),
        }
    };
    let flush = |outbox: &Rc<RefCell<Vec<String>>>| -> anyhow::Result<()> {
        for message in outbox.borrow_mut().drain(..) {
            let value: Json = serde_json::from_str(&message).unwrap_or_else(|_| json!({"t": "log", "text": message, "level": "warn"}));
            emit(&value)?;
        }
        Ok(())
    };

    // Install the prelude and start the script.
    sandbox.arm();
    let setup = sandbox.context.with(|ctx| -> Result<Persistent<Promise<'static>>, Json> {
        let sink = outbox.clone();
        let send = Function::new(ctx.clone(), move |message: String| {
            sink.borrow_mut().push(message);
        })
        .map_err(|e| error_json("internal", e.to_string()))?;
        let globals = ctx.globals();
        globals.set("__send", send).map_err(|e| error_json("internal", e.to_string()))?;
        let config = json!({"args": args, "budgetTotal": budget_total, "spent": spent, "maxItems": MAX_ITEMS});
        globals.set("__config", config.to_string()).map_err(|e| error_json("internal", e.to_string()))?;
        let _: Value = ctx.eval(PRELUDE).catch(&ctx).map_err(|e| error_json("internal", format!("prelude: {e}")))?;
        let mut options = rquickjs::context::EvalOptions::default();
        options.filename = Some(SCRIPT_FILE.to_string());
        options.strict = true;
        let function: Function = ctx
            .eval_with_options(body.as_bytes().to_vec(), options)
            .catch(&ctx)
            .map_err(|e| error_json("script_syntax", e.to_string()))?;
        let promise: Promise = function.call(()).catch(&ctx).map_err(|e| error_json("script_error", e.to_string()))?;
        Ok(Persistent::save(&ctx, promise))
    });
    sandbox.disarm();
    let promise = match setup {
        Ok(promise) => promise,
        Err(error) => {
            flush(&outbox)?;
            if sandbox.tripped.load(Ordering::Relaxed) {
                return finish(Err(error_json("script_cpu_limit", format!("the script ran more than {} s without waiting for an agent", CPU_SLICE.as_secs()))));
            }
            return finish(Err(error));
        }
    };

    loop {
        if let Err(message) = sandbox.pump() {
            flush(&outbox)?;
            return finish(Err(error_json("script_error", message)));
        }
        flush(&outbox)?;
        if sandbox.tripped.load(Ordering::Relaxed) {
            return finish(Err(error_json("script_cpu_limit", format!("the script ran more than {} s without waiting for an agent", CPU_SLICE.as_secs()))));
        }
        // Settled?
        let state = sandbox.context.with(|ctx| -> Result<Option<Result<Json, Json>>, Json> {
            let promise = promise.clone().restore(&ctx).map_err(|e| error_json("internal", e.to_string()))?;
            match promise.state() {
                PromiseState::Pending => {
                    let pending: i32 = ctx.eval("__pendingCount()").map_err(|e| error_json("internal", e.to_string()))?;
                    if pending == 0 {
                        return Ok(Some(Err(error_json("script_stalled", "the script is waiting on a promise that nothing will resolve"))));
                    }
                    Ok(None)
                }
                PromiseState::Resolved => {
                    let value: Value = promise.result::<Value>().and_then(|r| r.ok()).unwrap_or_else(|| Value::new_undefined(ctx.clone()));
                    if value.is_undefined() {
                        return Ok(Some(Ok(Json::Null)));
                    }
                    let text = ctx.json_stringify(value).catch(&ctx).map_err(|e| error_json("result_not_json", e.to_string()))?;
                    let text = match text {
                        Some(text) => text.to_string().map_err(|e| error_json("result_not_json", e.to_string()))?,
                        None => return Ok(Some(Ok(Json::Null))),
                    };
                    Ok(Some(serde_json::from_str(&text).map_err(|e| error_json("result_not_json", e.to_string()))))
                }
                PromiseState::Rejected => {
                    let _ = promise.result::<Value>();
                    let reason = ctx.catch();
                    let describe: Function = ctx.globals().get("__describe").map_err(|e| error_json("internal", e.to_string()))?;
                    let described: Value = describe.call((reason,)).map_err(|e| error_json("internal", e.to_string()))?;
                    let text = ctx.json_stringify(described).ok().flatten().and_then(|s| s.to_string().ok()).unwrap_or_default();
                    let error: Json = serde_json::from_str(&text).unwrap_or_else(|_| error_json("script_error", "the script threw"));
                    Ok(Some(Err(error)))
                }
            }
        });
        match state {
            Err(error) => return finish(Err(error)),
            Ok(Some(result)) => return finish(result),
            Ok(None) => {}
        }
        // Wait for the parent.
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(()); // parent went away: stop quietly
        }
        let message: Json = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(_) => continue,
        };
        sandbox.arm();
        let delivered = sandbox.context.with(|ctx| -> Result<(), String> {
            let deliver: Function = ctx.globals().get("__deliver").map_err(|e| e.to_string())?;
            deliver.call::<_, ()>((message.to_string(),)).catch(&ctx).map_err(|e| e.to_string())?;
            Ok(())
        });
        sandbox.disarm();
        if let Err(message) = delivered {
            return finish(Err(error_json("internal", message)));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{BufReader, Read};
    use std::sync::mpsc;

    /// Run a checked script in-process; `answer` maps agent requests to results.
    pub fn run_script(script: &str, args: Json, mut answer: impl FnMut(&Json) -> Json) -> (Json, Vec<Json>) {
        let checked = crate::workflows::script::check(script).expect("script checks");
        let (to_child, child_in) = os_pipe();
        let (child_out, from_child) = os_pipe();
        let body = checked.body.clone();
        let handle = std::thread::spawn(move || {
            run(BufReader::new(child_in), child_out).unwrap();
        });
        let mut to_child = to_child;
        writeln!(to_child, "{}", json!({"t": "start", "body": body, "args": args, "budgetTotal": 1000, "spent": 0})).unwrap();
        let mut lines = BufReader::new(from_child).lines();
        let mut seen = Vec::new();
        let mut spent = 0u64;
        loop {
            let line = lines.next().expect("child line").unwrap();
            let message: Json = serde_json::from_str(&line).unwrap();
            seen.push(message.clone());
            match message["t"].as_str().unwrap() {
                "agent" => {
                    spent += 10;
                    let mut reply = answer(&message);
                    reply["t"] = json!("result");
                    reply["id"] = message["id"].clone();
                    reply["spent"] = json!(spent);
                    writeln!(to_child, "{reply}").unwrap();
                }
                "done" => {
                    drop(to_child);
                    handle.join().unwrap();
                    return (message, seen);
                }
                _ => {}
            }
        }
    }

    /// A pipe made of a channel (portable, no extra crate).
    pub fn os_pipe() -> (PipeWriter, PipeReader) {
        let (tx, rx) = mpsc::channel();
        (PipeWriter(tx), PipeReader { rx, buffer: Vec::new(), at: 0 })
    }
    pub struct PipeWriter(mpsc::Sender<Vec<u8>>);
    impl Write for PipeWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.send(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    pub struct PipeReader {
        rx: mpsc::Receiver<Vec<u8>>,
        buffer: Vec<u8>,
        at: usize,
    }
    impl Read for PipeReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.at >= self.buffer.len() {
                match self.rx.recv() {
                    Ok(next) => {
                        self.buffer = next;
                        self.at = 0;
                    }
                    Err(_) => return Ok(0),
                }
            }
            let n = buf.len().min(self.buffer.len() - self.at);
            buf[..n].copy_from_slice(&self.buffer[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    const HEAD: &str = "export const meta = { name: 't', description: 'd' }\n";

    #[test]
    fn agent_parallel_pipeline_and_return() {
        let script = format!(
            "{HEAD}phase('One')\nconst a = await agent('hello', {{label: 'first'}})\nconst xs = await parallel([1,2,3].map(i => () => agent('p' + i)))\nconst ys = await pipeline(['a','b'], (prev, item, i) => agent('s1 ' + prev), (prev, item, i) => prev + '/' + item + i)\nlog('done ' + xs.length)\nreturn {{a, xs, ys, args}}\n"
        );
        let (done, seen) = run_script(&script, json!({"k": 1}), |request| {
            let prompt = request["prompt"].as_str().unwrap();
            if prompt == "p2" {
                json!({"ok": true, "value": null})
            } else {
                json!({"ok": true, "value": format!("re:{prompt}")})
            }
        });
        assert_eq!(done["ok"], json!(true), "{done}");
        let value = &done["value"];
        assert_eq!(value["a"], json!("re:hello"));
        assert_eq!(value["xs"], json!(["re:p1", null, "re:p3"]));
        assert_eq!(value["ys"], json!(["re:s1 a/a0", "re:s1 b/b1"]));
        assert_eq!(value["args"], json!({"k": 1}));
        let first = seen.iter().find(|m| m["t"] == "agent").unwrap();
        assert_eq!(first["opts"]["phase"], json!("One"));
        assert_eq!(first["opts"]["label"], json!("first"));
        assert!(seen.iter().any(|m| m["t"] == "log" && m["text"] == "done 3"));
    }

    #[test]
    fn errors_from_the_parent_throw_and_parallel_turns_them_into_null() {
        let script = format!(
            "{HEAD}const xs = await parallel([() => agent('a'), () => agent('b')])\ntry {{ await agent('c') }} catch (e) {{ return {{xs, code: e.code}} }}\n"
        );
        let (done, _) = run_script(&script, Json::Null, |request| {
            if request["prompt"] == "a" {
                json!({"ok": true, "value": 1})
            } else {
                json!({"ok": false, "error": {"code": "agent_limit", "message": "too many"}})
            }
        });
        assert_eq!(done["value"], json!({"xs": [1, null], "code": "agent_limit"}), "{done}");
    }

    #[test]
    fn determinism_guards_throw() {
        for expr in ["Date.now()", "Math.random()", "new Date()", "Date()"] {
            let script = format!("{HEAD}return {expr}\n");
            let (done, _) = run_script(&script, Json::Null, |_| json!({"ok": true, "value": 0}));
            assert_eq!(done["ok"], json!(false), "{expr}: {done}");
            assert!(done["error"]["message"].as_str().unwrap().contains("resume"), "{expr}: {done}");
        }
        let script = format!("{HEAD}return new Date(0).toISOString()\n");
        let (done, _) = run_script(&script, Json::Null, |_| json!({"ok": true, "value": 0}));
        assert_eq!(done["value"], json!("1970-01-01T00:00:00.000Z"), "{done}");
    }

    #[test]
    fn uncaught_errors_cpu_limit_and_stalls_end_the_run() {
        let (done, _) = run_script(&format!("{HEAD}throw new Error('boom')\n"), Json::Null, |_| json!({}));
        assert_eq!(done["error"]["code"], json!("script_error"));
        assert!(done["error"]["message"].as_str().unwrap().contains("boom"), "{done}");
        let (done, _) = run_script(&format!("{HEAD}await new Promise(() => {{}})\n"), Json::Null, |_| json!({}));
        assert_eq!(done["error"]["code"], json!("script_stalled"), "{done}");
        let (done, _) = run_script(&format!("{HEAD}typeof setTimeout === 'undefined' && typeof require === 'undefined' ? 0 : (() => {{ throw new Error('host api') }})()\nreturn 'sandboxed'\n"), Json::Null, |_| json!({}));
        assert_eq!(done["value"], json!("sandboxed"), "{done}");
    }

    #[test]
    #[ignore = "takes CPU_SLICE (5 s)"]
    fn busy_loops_hit_the_cpu_limit() {
        let (done, _) = run_script(&format!("{HEAD}while (true) {{}}\n"), Json::Null, |_| json!({}));
        assert_eq!(done["error"]["code"], json!("script_cpu_limit"), "{done}");
    }
}
