//! One scenario's world: private home + seeded agents on fake runtimes +
//! daemon + recorder. `finish` saves the trace as JSONL (the L3 fixture) and
//! runs the §5.2 common assertion.
#![allow(dead_code)]

use crate::daemon::{seed_sessions, Daemon, DaemonOptions, PrivateHome, Recorder, SeededAgentSpec, Sleeper};
use crate::fake_runtime::{FakeRuntime, Kind, Script};
use crate::trace::{self, AssertOptions, Trace};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Agent {
    pub sid: String,
    pub kind: Kind,
    pub runtime_agent_id: String,
    pub fake: FakeRuntime,
    sleeper: Sleeper,
}

pub struct World {
    // Drop order: recorder, daemon (its PIDs only), then the fakes.
    pub recorder: Recorder,
    pub daemon: Daemon,
    pub agents: Vec<Agent>,
    pub name: String,
}

pub struct WorldSpec {
    pub kinds: Vec<Kind>,
    pub scripts: Vec<Script>,
    pub options: DaemonOptions,
    /// Agents whose fake starts offline (refuses connections) by index.
    pub offline: Vec<usize>,
    pub host_stream: bool,
    /// Agents seeded as persistent agents (delegation initiators) by index.
    pub persistent: Vec<usize>,
    /// Agents seeded on a Claude profile with `delegation_wait` on, by index.
    pub delegation_wait_profile: Vec<usize>,
}

impl WorldSpec {
    pub fn new(kinds: &[Kind]) -> Self {
        Self { kinds: kinds.to_vec(), scripts: Vec::new(), options: DaemonOptions::default(), offline: Vec::new(), host_stream: true, persistent: Vec::new(), delegation_wait_profile: Vec::new() }
    }

    pub fn bus(mut self) -> Self {
        self.options.bus = true;
        self.options.fake_external_input = true;
        self
    }

    /// 057 only: advertise ownerInputCarries:1 on this fake's initialize.
    pub fn owner_carries(mut self, index: usize) -> Self {
        if self.scripts.len() <= index { self.scripts.resize_with(index + 1, Script::default); }
        self.scripts[index].owner_input_carries = true;
        self
    }

    pub fn ledger(mut self, enabled: bool) -> Self {
        self.options.ledger_enabled = enabled;
        self
    }

    pub fn script(mut self, index: usize, script: Script) -> Self {
        if self.scripts.len() <= index {
            self.scripts.resize_with(index + 1, Script::default);
        }
        self.scripts[index] = script;
        self
    }

    pub fn persistent(mut self, index: usize) -> Self {
        self.persistent.push(index);
        self
    }

    /// Seed agent `index` on a Claude profile whose `delegation_wait` is on
    /// (a Claude profile defaults it off: tool_policy.rs
    /// `delegation_wait_enabled`, settings_api.rs `default_delegation_wait`).
    pub fn delegation_wait_profile(mut self, index: usize) -> Self {
        self.delegation_wait_profile.push(index);
        self
    }

    /// Stall check of a quiet bound turn after `secs` instead of 5 min.
    pub fn turn_stall_secs(mut self, secs: u64) -> Self {
        self.options.turn_stall_secs = Some(secs);
        self
    }

    pub fn offline(mut self, index: usize) -> Self {
        self.offline.push(index);
        self
    }
}

impl Drop for World {
    // A failing scenario still leaves its trace for C2–C5 debugging.
    fn drop(&mut self) {
        if !self.trace_path().exists() {
            let _ = self.recorder.snapshot().save(&self.trace_path());
        }
    }
}

impl World {
    pub fn start(name: &str, spec: WorldSpec) -> Self {
        let home = PrivateHome::create(name);
        let workspace = home.workspace.display().to_string();
        let mut fakes = Vec::new();
        let mut sleepers = Vec::new();
        let mut specs = Vec::new();
        for (index, kind) in spec.kinds.iter().enumerate() {
            let mut script = spec.scripts.get(index).cloned().unwrap_or_default();
            if spec.offline.contains(&index) {
                script.refuse_connections = true;
            }
            fakes.push(FakeRuntime::start_with(*kind, &workspace, script));
            sleepers.push(match kind {
                // The bound PID serves the endpoint (r273.1 ownership check).
                #[cfg(not(unix))]
                Kind::Codex => Sleeper::forwarding(fakes[index].port),
                #[cfg(unix)]
                Kind::Codex => Sleeper::forwarding_in(fakes[index].port,
                    home.config_dir.join("runtime/app-server").join(format!("il-e2e-{index}-codex"))),
                Kind::Claude => Sleeper::spawn(),
            });
            specs.push(SeededAgentSpec {
                kind: *kind,
                // Claude: the bus registration must match the adapter occurrence
                // (session_reconcile.rs `claude.<connection id>`).
                runtime_agent_id: match kind {
                    Kind::Claude => format!("claude.{}", fakes[index].connection_id),
                    Kind::Codex => format!("il-e2e-{name}-{index}-{}", kind.name()).replace('_', "-"),
                },
                thread_name: Some(format!("{name}-{index}")),
                persistent: spec.persistent.contains(&index),
                profile: spec.delegation_wait_profile.contains(&index).then(|| crate::daemon::DELEGATION_WAIT_PROFILE.to_string()),
            });
        }
        let seeds: Vec<_> = specs.iter().zip(&fakes).zip(&sleepers).map(|((s, f), p)| (s, f, p)).collect();
        let sids = seed_sessions(&home, &seeds);
        drop(seeds);
        let mut daemon = Daemon::configure(home, spec.options);
        daemon.start();
        let mut agents = Vec::new();
        for (((sid, fake), sleeper), spec_) in sids.into_iter().zip(fakes).zip(sleepers).zip(specs) {
            agents.push(Agent { sid, kind: spec_.kind, runtime_agent_id: spec_.runtime_agent_id, fake, sleeper });
        }
        for (index, agent) in agents.iter().enumerate() {
            if !spec.offline.contains(&index) {
                daemon.wait_connected(&agent.sid, CONNECT_TIMEOUT);
            }
        }
        let mut recorder = Recorder::new();
        for agent in &agents {
            recorder.subscribe(&daemon, "session", Some(&agent.sid));
        }
        if spec.host_stream {
            recorder.subscribe(&daemon, "host", None);
        }
        Self { name: name.to_string(), agents, recorder, daemon }
    }

    pub fn agent(&self, index: usize) -> &Agent {
        &self.agents[index]
    }

    pub fn sid(&self, index: usize) -> &str {
        &self.agents[index].sid
    }

    pub fn fake(&self, index: usize) -> &FakeRuntime {
        &self.agents[index].fake
    }

    /// 057: real daemon stopHold; the fake turn supplies the interrupt target.
    pub fn stop_owner(&self, index: usize) {
        let result = self.daemon.cutex(self.sid(index), "cutex/turn/stop", json!({})).expect("owner stop");
        self.recorder.mark("ownerStop057", result.clone());
        // `result.paused` describes only the numbered owner queue, and is
        // false when that queue is empty. The actual session stopHold is
        // independently committed in AskStore before the interrupt.
        // The host AskStore keeps its rows in the shared management-v2 DB;
        // `AskStore::at(runtime/asks)` would read an absent isolated-store DB
        // and silently return an empty snapshot. This handle is read-only.
        let source = self.daemon.home.config_dir.join("runtime").join("management-v2");
        assert!(source.join("management-state.sqlite3").is_file(),
            "private shared AskStore database must exist: {}", source.display());
        let asks = cutex::asks::store::AskStore::at(source)
            .readonly_history_snapshot().expect("read private authoritative stopHold");
        assert!(cutex::asks::control::stop_held(&asks, self.sid(index)),
            "owner stop must persist its session stopHold even with an empty owner queue: {result}");
        self.daemon.wait_active_turn(self.sid(index), false, Duration::from_secs(20));
    }

    pub fn resume_owner(&self, index: usize) {
        let result = self.daemon.cutex(self.sid(index), "cutex/userInput/queue/resume", json!({})).expect("owner resume");
        self.recorder.mark("ownerResume057", result);
    }

    pub fn input_record(&self, index: usize, id: &str) -> Value {
        let got = self.daemon.cutex(self.sid(index), "cutex/input/get", json!({"inputIds": [id]})).expect("input/get");
        got["records"].as_array().and_then(|records| records.iter().find(|record| record["inputId"] == id))
            .unwrap_or_else(|| panic!("{id}: absent from input/get: {got}")).clone()
    }

    pub fn hold_input(&self, index: usize, id: &str, held: bool) -> Value {
        let before = self.input_record(index, id);
        let action = if held { "hold" } else { "releaseHold" };
        assert!(before["actions"].as_array().is_some_and(|actions| actions.iter().any(|value| value == action)),
            "source authority must offer {action}: {before}");
        let result = self.daemon.cutex(self.sid(index), "cutex/input/hold",
            json!({"inputId": id, "expectedRev": before["rev"], "held": held})).expect("input/hold");
        self.recorder.mark("ownerHold057", result.clone());
        assert_eq!(result["record"]["details"]["ownerHold"], held, "{result}");
        let next_action = if held { "releaseHold" } else { "hold" };
        assert!(result["record"]["actions"].as_array().is_some_and(|actions| actions.iter().any(|value| value == next_action)),
            "source-published hold result must offer {next_action}: {result}");
        result["record"].clone()
    }

    /// Observe only this world's persisted source, never the process default home.
    pub fn source_owner_blocked(&self, index: usize, id: &str) -> bool {
        let root = self.daemon.home.config_dir.join("runtime").join("management-v2");
        cutex::management::v2::agent_bus_state::AgentBusMessageRepository::open_read_only(root)
            .owner_delivery_blocked(self.sid(index), id).expect("read private bus source")
    }

    /// Service input created through the real daemon ingress: a stopped Claude
    /// Task notification, or a Job Service completion for the Codex fixture.
    pub fn system_notice(&self, index: usize, marker: &str) {
        if self.agent(index).kind == Kind::Claude {
            let task = format!("057-{}", uuid::Uuid::new_v4().simple());
            self.fake(index).emit("claude/task/updated", json!({"threadId": self.fake(index).thread,
                "task": {"background": true, "taskId": task, "taskType": "local_agent", "status": "stopped",
                    "notifyParent": {"noticeId": task, "taskId": task, "taskType": "local_agent",
                        "status": "stopped", "description": marker, "reason": "owner stopped"}}}));
        } else {
            let (status, receipt) = self.job_notice(index, marker);
            assert_eq!(status, 200, "{receipt}");
        }
    }

    /// Owner `cutex/userInput/submit`; `extra` is merged into the params.
    pub fn submit(&self, index: usize, client_id: &str, strategy: &str, extra: Value) -> Result<Value, Value> {
        let mut params = json!({
            "clientUserMessageId": client_id,
            "origin": {"kind": "other", "clientId": "input-ledger-e2e"},
            "strategy": strategy,
            "input": [{"type": "text", "text": client_id, "text_elements": []}],
        });
        if let (Some(target), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                target.insert(k.clone(), v.clone());
            }
        }
        let result = self.daemon.cutex(self.sid(index), "cutex/userInput/submit", params);
        self.recorder.mark("ownerSubmit", json!({"sid": self.sid(index), "clientUserMessageId": client_id, "strategy": strategy, "result": result}));
        result
    }

    /// A bus message (`cutex agent send`) from agent `from` (its runtime
    /// agent id as `CUTEX_AGENT_ID`) to agent `index`; returns the CLI output.
    pub fn bus_send(&self, from: usize, index: usize, text: &str, flags: &[&str]) -> std::process::Output {
        let mut args = vec!["agent", "send"];
        args.extend_from_slice(flags);
        args.push(self.sid(index));
        args.push(text);
        let mut command = self.daemon.command(&args);
        command.env("CUTEX_AGENT_ID", &self.agent(from).runtime_agent_id);
        let out = command.output().expect("run cutex agent send");
        self.recorder.mark("busSend", json!({"from": self.sid(from), "to": self.sid(index), "text": text, "flags": flags,
            "status": out.status.code(), "stdout": String::from_utf8_lossy(&out.stdout), "stderr": String::from_utf8_lossy(&out.stderr)}));
        out
    }

    pub fn alarm(&self, index: usize, text: &str, after_secs: u64) -> std::process::Output {
        let after = after_secs.to_string();
        let out = self.daemon.cli(&["alarm", "set", "--to", self.sid(index), "--message", text, "--after", &after]);
        self.recorder.mark("alarmSet", json!({"to": self.sid(index), "status": out.status.code(),
            "stderr": String::from_utf8_lossy(&out.stderr)}));
        out
    }

    /// A Job Service completion for agent `index` (must be persistent), the
    /// way the Job Service reports one: `POST /api/job-service/v1/completions`
    /// on this daemon's Agent Bus with the completion credential the bus
    /// keeps in `<runtime>/task-service/job-service-completion.token`
    /// (schema v1). Returns (HTTP status, receipt).
    pub fn job_notice(&self, index: usize, summary: &str) -> (u16, Value) {
        let path = self.daemon.home.config_dir.join("runtime").join("task-service").join("job-service-completion.token");
        let token = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let body = json!({
            "schema": "cutex.job_service.completion.v1",
            "eventId": format!("il-e2e-job-{}", uuid::Uuid::new_v4().simple()),
            "jobId": format!("il-e2e-job-{}", self.name),
            "jobRevision": 1,
            "terminalStatus": "exited",
            "resultSha256": "a".repeat(64),
            "targetCutexSessionId": self.sid(index),
            "summary": summary,
        });
        let (status, receipt) = crate::daemon::http_request(self.daemon.bus_port, token.trim(), "POST", "/api/job-service/v1/completions", Some(&body))
            .expect("job service completion");
        self.recorder.mark("jobNotice", json!({"to": self.sid(index), "status": status, "receipt": receipt}));
        (status, receipt)
    }

    /// An MCP tool call made by agent `from` (its identity, generation 1 as
    /// seeded, its native thread), recorded as a marker.
    pub fn mcp(&self, from: usize, tool: &str, arguments: Value) -> Result<Value, String> {
        let agent = self.agent(from);
        let result = self.daemon.mcp_call(&agent.runtime_agent_id, 1, &agent.fake.thread, tool, arguments.clone());
        self.recorder.mark("mcpCall", json!({"from": agent.sid, "tool": tool, "arguments": arguments,
            "result": match &result { Ok(v) => v.clone(), Err(e) => json!({"error": e}) }}));
        result
    }

    /// Wait until the Agent Asks presence (`agent_list` status, refreshed
    /// every few scheduler ticks) sees agents `indices` online: right after
    /// the daemon starts it can still hold the pre-connect view, and asks
    /// would then treat a connected agent as offline (`waiting{agentOffline}`).
    /// Not recorded as a marker. `agent_list` needs a persistent caller; a
    /// world without one skips the wait. Rows may elide ids (`cutex.…<tail>`).
    pub fn wait_asks_presence(&self, indices: &[usize]) {
        let until = Instant::now() + CONNECT_TIMEOUT;
        let list = |caller: usize| {
            let agent = self.agent(caller);
            self.daemon.mcp_call(&agent.runtime_agent_id, 1, &agent.fake.thread, "agent_list", json!({}))
                .ok().and_then(|v| v["agents"].as_array().cloned())
        };
        let Some(caller) = (0..self.agents.len()).find(|i| list(*i).is_some()) else { return };
        let same = |row: &Value, sid: &str| match row["id"].as_str() {
            Some(id) if id == sid => true,
            Some(id) => id.split_once('…').is_some_and(|(head, tail)| !tail.is_empty() && sid.starts_with(head) && sid.ends_with(tail)),
            None => false,
        };
        for &index in indices {
            let sid = self.sid(index).to_string();
            loop {
                let rows = list(caller).unwrap_or_default();
                let status = rows.iter().find(|row| same(row, &sid)).map(|row| row["status"].clone()).unwrap_or(Value::Null);
                // The caller itself is not listed: its own presence shows up
                // through another caller (or not at all with one agent).
                if index == caller || (status.is_string() && status != "offline") {
                    break;
                }
                assert!(Instant::now() < until, "{}: asks presence of {sid} never online: {status} ({rows:?})", self.name);
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        if indices.contains(&caller) {
            if let Some(other) = (0..self.agents.len()).find(|i| *i != caller && list(*i).is_some()) {
                let sid = self.sid(caller).to_string();
                loop {
                    let status = list(other).unwrap_or_default().iter().find(|row| same(row, &sid)).map(|row| row["status"].clone()).unwrap_or(Value::Null);
                    if status.is_string() && status != "offline" {
                        break;
                    }
                    assert!(Instant::now() < until, "{}: asks presence of {sid} never online: {status}", self.name);
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }

    pub fn trace(&self) -> Trace {
        self.recorder.snapshot()
    }

    /// Poll the recorded trace until `done` holds; false on timeout.
    pub fn wait(&self, timeout: Duration, mut done: impl FnMut(&Trace) -> bool) -> bool {
        let until = Instant::now() + timeout;
        loop {
            if done(&self.trace()) {
                return true;
            }
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait until the latest record of every id is settled.
    #[track_caller]
    pub fn wait_settled(&self, ids: &[&str], timeout: Duration) {
        let ok = self.wait(timeout, |t| {
            ids.iter().all(|id| t.latest(id).is_some_and(|r| trace::is_settled(r["state"].as_str().unwrap_or(""))))
        });
        if !ok {
            let t = self.trace();
            let paths: Vec<_> = ids.iter().map(|id| (id.to_string(), t.path(id))).collect();
            panic!("{}: not settled within {timeout:?}: {paths:?} (trace {})", self.name, self.trace_path().display());
        }
    }

    #[track_caller]
    pub fn wait_state(&self, id: &str, label: &str, timeout: Duration) {
        let ok = self.wait(timeout, |t| t.path(id).iter().any(|l| trace::label_matches(label, l)));
        assert!(ok, "{}: {id} never reached {label}: {:?}", self.name, self.trace().path(id));
    }

    pub fn trace_path(&self) -> std::path::PathBuf {
        self.daemon.home.root.join("trace.jsonl")
    }

    /// Final reads, save JSONL (also to `INPUT_LEDGER_E2E_TRACE_DIR/<name>.jsonl`),
    /// then the common assertion.
    #[track_caller]
    pub fn finish(mut self, options: AssertOptions) -> Trace {
        let _ = std::fs::remove_file(self.trace_path());
        for agent in &self.agents {
            let _ = self.recorder.read_input_list(&self.daemon, &agent.sid);
            let _ = self.recorder.read_queue_list(&self.daemon, &agent.sid);
        }
        std::thread::sleep(Duration::from_millis(300));
        let trace = self.recorder.stop();
        trace.save(&self.trace_path()).unwrap();
        if let Some(dir) = std::env::var_os("INPUT_LEDGER_E2E_TRACE_DIR") {
            trace.save(&std::path::Path::new(&dir).join(format!("{}.jsonl", self.name))).unwrap();
        }
        if let Err(errors) = trace::check_surface_trace(&trace, options) {
            // Same message as `assert_surface_trace_with`, plus the evidence of
            // every deadline miss (record history, route, Management slow lines).
            let log = std::fs::read_to_string(self.daemon.home.root.join("logs").join("management.log")).ok();
            let report = trace::deadline_report(&trace, &errors, log.as_deref());
            panic!("surface trace violates 054 §5.2:
  {}{report}
(trace {})", errors.join("
  "), self.trace_path().display());
        }
        trace
    }
}
